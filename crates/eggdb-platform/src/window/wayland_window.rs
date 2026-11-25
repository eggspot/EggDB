use super::wayland::{self, Arguments, Event, Request, event, request};
use super::{KeyPress, Modifiers, PointerButton, Window, WindowError, WindowEvent, WindowOptions};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const EGGOS_DISPLAY: &str = "/run/user/0/wayland-0";
const APP_ID: &str = "eggdb";
const WHEEL_UNITS_PER_LINE: f32 = 10.0;
const READ_BYTES: usize = 4096;

mod fd_passing {
    use std::ffi::c_void;

    const SOL_SOCKET: i32 = 1;
    const SCM_RIGHTS: i32 = 1;
    const MSG_NOSIGNAL: i32 = 0x4000;

    #[repr(C)]
    struct IoVector {
        base: *const c_void,
        length: usize,
    }

    #[repr(C)]
    struct MessageHeader {
        name: *mut c_void,
        name_length: u32,
        vectors: *const IoVector,
        vector_count: usize,
        control: *mut c_void,
        control_length: usize,
        flags: i32,
    }

    unsafe extern "C" {
        fn sendmsg(socket: i32, message: *const MessageHeader, flags: i32) -> isize;
    }

    fn aligned(length: usize) -> usize {
        length.div_ceil(size_of::<usize>()) * size_of::<usize>()
    }

    pub(super) fn send_with_fd(socket: i32, bytes: &[u8], fd: i32) -> std::io::Result<()> {
        let header_length = size_of::<usize>() + 8;
        let control_length = aligned(header_length) + aligned(size_of::<i32>());
        let mut control = [0u64; 4];
        let mut written = Vec::with_capacity(32);
        written.extend_from_slice(&(aligned(header_length) + size_of::<i32>()).to_ne_bytes());
        written.extend_from_slice(&SOL_SOCKET.to_ne_bytes());
        written.extend_from_slice(&SCM_RIGHTS.to_ne_bytes());
        written.resize(aligned(header_length), 0);
        written.extend_from_slice(&fd.to_ne_bytes());
        written.resize(32, 0);
        for (word, bytes) in control.iter_mut().zip(written.chunks_exact(8)) {
            *word = u64::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]);
        }
        let vector = IoVector {
            base: bytes.as_ptr().cast(),
            length: bytes.len(),
        };
        let header = MessageHeader {
            name: std::ptr::null_mut(),
            name_length: 0,
            vectors: &raw const vector,
            vector_count: 1,
            control: control.as_mut_ptr().cast(),
            control_length,
            flags: 0,
        };
        // SAFETY: the header, its one vector over `bytes` and its control buffer all live
        // until the call returns, laid out as Linux's `msghdr`, `iovec` and `cmsghdr`.
        let sent = unsafe { sendmsg(socket, &raw const header, MSG_NOSIGNAL) };
        match usize::try_from(sent) {
            Ok(count) if count == bytes.len() => Ok(()),
            Ok(_) => Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "a short write of a file descriptor's message")),
            Err(_) => Err(std::io::Error::last_os_error()),
        }
    }
}

fn io_error(error: &std::io::Error) -> WindowError {
    WindowError::Io(error.to_string())
}

fn protocol(reason: impl Into<String>) -> WindowError {
    WindowError::Protocol(reason.into())
}

/// Where the compositor listens: `$WAYLAND_DISPLAY`, a path or a name in `$XDG_RUNTIME_DIR`,
/// or EggOS's `eggway` socket on EggOS.
pub(crate) fn display_path() -> Option<PathBuf> {
    match std::env::var_os("WAYLAND_DISPLAY") {
        Some(name) => {
            let name = PathBuf::from(name);
            if name.is_absolute() {
                return Some(name);
            }
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
            Some(PathBuf::from(runtime).join(name))
        }
        None if crate::os_identity::on_eggos() => Some(PathBuf::from(EGGOS_DISPLAY)),
        None => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct ShmBuffer {
    id: u32,
    offset: u64,
    width: u32,
    height: u32,
    busy: bool,
}

#[derive(Debug, Default)]
struct Repeat {
    rate: u32,
    delay: Duration,
    held: Option<(KeyPress, Instant)>,
}

pub(crate) struct WaylandWindow {
    stream: UnixStream,
    incoming: Vec<u8>,
    next_id: u32,
    registry_id: u32,
    compositor: u32,
    shm: u32,
    wm_base: u32,
    seat: Option<u32>,
    output: Option<u32>,
    surface: u32,
    xdg_surface: u32,
    toplevel: u32,
    pointer: Option<u32>,
    keyboard: Option<u32>,
    logical: (u32, u32),
    pending: Option<(u32, u32)>,
    scale: u32,
    configured: bool,
    pool: Option<(u32, File, u64)>,
    buffers: Vec<ShmBuffer>,
    events: VecDeque<WindowEvent>,
    pointer_at: (f32, f32),
    modifiers: Modifiers,
    caps_lock: bool,
    repeat: Repeat,
    pixels: Vec<u8>,
    pool_number: u32,
}

impl std::fmt::Debug for WaylandWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WaylandWindow")
            .field("surface", &self.surface)
            .field("logical", &self.logical)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

struct Globals {
    compositor: Option<(u32, u32)>,
    shm: Option<(u32, u32)>,
    wm_base: Option<(u32, u32)>,
    seat: Option<(u32, u32)>,
    output: Option<(u32, u32)>,
}

impl WaylandWindow {
    pub(crate) fn open(options: &WindowOptions) -> Result<Self, WindowError> {
        let path = display_path().ok_or_else(|| WindowError::NoDisplay("WAYLAND_DISPLAY is not set".to_owned()))?;
        let stream = UnixStream::connect(&path)
            .map_err(|error| WindowError::NoDisplay(format!("{}: {error}", path.display())))?;
        let mut window = Self {
            stream,
            incoming: Vec::new(),
            next_id: 2,
            registry_id: 0,
            compositor: 0,
            shm: 0,
            wm_base: 0,
            seat: None,
            output: None,
            surface: 0,
            xdg_surface: 0,
            toplevel: 0,
            pointer: None,
            keyboard: None,
            logical: (options.width.max(1), options.height.max(1)),
            pending: None,
            scale: 1,
            configured: false,
            pool: None,
            buffers: Vec::new(),
            events: VecDeque::new(),
            pointer_at: (0.0, 0.0),
            modifiers: Modifiers::default(),
            caps_lock: false,
            repeat: Repeat::default(),
            pixels: Vec::new(),
            pool_number: 0,
        };
        let globals = window.discover_globals()?;
        window.bind_globals(&globals)?;
        window.create_toplevel(options)?;
        while !window.configured {
            window.read_events(None)?;
        }
        Ok(window)
    }

    fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, request: Request) -> Result<(), WindowError> {
        self.stream.write_all(&request.finish()).map_err(|error| io_error(&error))
    }

    fn roundtrip(&mut self) -> Result<Vec<Event>, WindowError> {
        let callback = self.new_id();
        self.send(Request::new(wayland::DISPLAY_ID, request::DISPLAY_SYNC).uint(callback))?;
        let mut seen = Vec::new();
        loop {
            let read = self.read_raw(None)?;
            for found in read {
                if found.object == callback && found.opcode == event::CALLBACK_DONE {
                    return Ok(seen);
                }
                seen.push(found);
            }
        }
    }

    fn discover_globals(&mut self) -> Result<Globals, WindowError> {
        let registry = self.new_id();
        self.send(Request::new(wayland::DISPLAY_ID, request::DISPLAY_GET_REGISTRY).uint(registry))?;
        let mut globals = Globals {
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            output: None,
        };
        for found in self.roundtrip()? {
            if found.object != registry || found.opcode != event::REGISTRY_GLOBAL {
                continue;
            }
            let mut arguments = Arguments::of(&found);
            let name = arguments.uint().map_err(protocol)?;
            let interface = arguments.string().map_err(protocol)?;
            let version = arguments.uint().map_err(protocol)?;
            let slot = match interface.as_str() {
                "wl_compositor" => &mut globals.compositor,
                "wl_shm" => &mut globals.shm,
                "xdg_wm_base" => &mut globals.wm_base,
                "wl_seat" => &mut globals.seat,
                "wl_output" => &mut globals.output,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some((name, version));
            }
        }
        self.next_id = self.next_id.max(registry + 1);
        self.registry_id = registry;
        Ok(globals)
    }

    fn bind(&mut self, global: (u32, u32), interface: &str, most_version: u32) -> Result<u32, WindowError> {
        let id = self.new_id();
        let registry = self.registry_id;
        self.send(
            Request::new(registry, request::REGISTRY_BIND)
                .uint(global.0)
                .string(interface)
                .uint(global.1.min(most_version))
                .uint(id),
        )?;
        Ok(id)
    }

    fn bind_globals(&mut self, globals: &Globals) -> Result<(), WindowError> {
        let missing = |what: &str| WindowError::Unsupported(match what {
            "wl_compositor" => "the Wayland compositor offers no wl_compositor",
            "wl_shm" => "the Wayland compositor offers no shared-memory buffers (wl_shm)",
            _ => "the Wayland compositor offers no xdg_wm_base, so it cannot show application windows",
        });
        let compositor = globals.compositor.ok_or_else(|| missing("wl_compositor"))?;
        let shm = globals.shm.ok_or_else(|| missing("wl_shm"))?;
        let wm_base = globals.wm_base.ok_or_else(|| missing("xdg_wm_base"))?;
        self.compositor = self.bind(compositor, "wl_compositor", 4)?;
        self.shm = self.bind(shm, "wl_shm", 1)?;
        self.wm_base = self.bind(wm_base, "xdg_wm_base", 1)?;
        if let Some(seat) = globals.seat {
            self.seat = Some(self.bind(seat, "wl_seat", 5)?);
        }
        if let Some(output) = globals.output {
            self.output = Some(self.bind(output, "wl_output", 2)?);
        }
        Ok(())
    }

    fn create_toplevel(&mut self, options: &WindowOptions) -> Result<(), WindowError> {
        self.surface = self.new_id();
        let compositor = self.compositor;
        let surface = self.surface;
        self.send(Request::new(compositor, request::COMPOSITOR_CREATE_SURFACE).uint(surface))?;
        self.xdg_surface = self.new_id();
        let (wm_base, xdg_surface) = (self.wm_base, self.xdg_surface);
        self.send(Request::new(wm_base, request::WM_BASE_GET_XDG_SURFACE).uint(xdg_surface).uint(surface))?;
        self.toplevel = self.new_id();
        let toplevel = self.toplevel;
        self.send(Request::new(xdg_surface, request::XDG_SURFACE_GET_TOPLEVEL).uint(toplevel))?;
        self.send(Request::new(toplevel, request::TOPLEVEL_SET_TITLE).string(&options.title))?;
        self.send(Request::new(toplevel, request::TOPLEVEL_SET_APP_ID).string(APP_ID))?;
        let min_width = i32::try_from(options.min_width).unwrap_or(i32::MAX);
        let min_height = i32::try_from(options.min_height).unwrap_or(i32::MAX);
        self.send(Request::new(toplevel, request::TOPLEVEL_SET_MIN_SIZE).int(min_width).int(min_height))?;
        self.send(Request::new(surface, request::SURFACE_COMMIT))
    }

    fn read_raw(&mut self, timeout: Option<Duration>) -> Result<Vec<Event>, WindowError> {
        let mut found = Vec::new();
        while let Some(next) = wayland::next_event(&mut self.incoming).map_err(protocol)? {
            found.push(next);
        }
        if !found.is_empty() {
            return Ok(found);
        }
        let wait = timeout.map(|wait| wait.max(Duration::from_millis(1)));
        self.stream.set_read_timeout(wait).map_err(|error| io_error(&error))?;
        let mut chunk = [0u8; READ_BYTES];
        match self.stream.read(&mut chunk) {
            Ok(0) => return Err(WindowError::Io("the Wayland compositor closed the connection".to_owned())),
            Ok(count) => self.incoming.extend_from_slice(&chunk[..count]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(found),
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(found),
            Err(error) => return Err(io_error(&error)),
        }
        while let Some(next) = wayland::next_event(&mut self.incoming).map_err(protocol)? {
            found.push(next);
        }
        Ok(found)
    }

    fn read_events(&mut self, timeout: Option<Duration>) -> Result<(), WindowError> {
        for found in self.read_raw(timeout)? {
            self.handle(&found)?;
        }
        Ok(())
    }

    fn device(&self, value: f32) -> f32 {
        value * self.scale as f32
    }

    fn handle(&mut self, found: &Event) -> Result<(), WindowError> {
        let mut arguments = Arguments::of(found);
        let object = found.object;
        if object == wayland::DISPLAY_ID && found.opcode == event::DISPLAY_ERROR {
            let _ = arguments.uint();
            let code = arguments.uint().unwrap_or(0);
            let message = arguments.string().unwrap_or_default();
            return Err(protocol(format!("the compositor ended the session: error {code}: {message}")));
        }
        if object == self.wm_base && found.opcode == event::WM_BASE_PING {
            let serial = arguments.uint().map_err(protocol)?;
            let wm_base = self.wm_base;
            return self.send(Request::new(wm_base, request::WM_BASE_PONG).uint(serial));
        }
        if object == self.toplevel {
            match found.opcode {
                event::TOPLEVEL_CONFIGURE => {
                    let width = arguments.int().map_err(protocol)?;
                    let height = arguments.int().map_err(protocol)?;
                    if width > 0 && height > 0 {
                        self.pending = Some((width as u32, height as u32));
                    }
                }
                event::TOPLEVEL_CLOSE => self.events.push_back(WindowEvent::CloseRequested),
                _ => {}
            }
            return Ok(());
        }
        if object == self.xdg_surface && found.opcode == event::XDG_SURFACE_CONFIGURE {
            let serial = arguments.uint().map_err(protocol)?;
            let xdg_surface = self.xdg_surface;
            self.send(Request::new(xdg_surface, request::XDG_SURFACE_ACK_CONFIGURE).uint(serial))?;
            if let Some(size) = self.pending.take()
                && size != self.logical
            {
                self.logical = size;
                let (width, height) = self.size();
                self.events.push_back(WindowEvent::Resized { width, height });
            }
            self.configured = true;
            self.events.push_back(WindowEvent::Redraw);
            return Ok(());
        }
        if Some(object) == self.output && found.opcode == event::OUTPUT_SCALE {
            let factor = u32::try_from(arguments.int().map_err(protocol)?).unwrap_or(1).clamp(1, 8);
            if factor != self.scale {
                self.scale = factor;
                let surface = self.surface;
                self.send(Request::new(surface, request::SURFACE_SET_BUFFER_SCALE).int(factor as i32))?;
                self.events.push_back(WindowEvent::ScaleChanged(factor as f32));
                let (width, height) = self.size();
                self.events.push_back(WindowEvent::Resized { width, height });
            }
            return Ok(());
        }
        if Some(object) == self.seat && found.opcode == event::SEAT_CAPABILITIES {
            let capabilities = arguments.uint().map_err(protocol)?;
            let seat = object;
            if capabilities & wayland::SEAT_POINTER != 0 && self.pointer.is_none() {
                let pointer = self.new_id();
                self.send(Request::new(seat, request::SEAT_GET_POINTER).uint(pointer))?;
                self.pointer = Some(pointer);
            }
            if capabilities & wayland::SEAT_KEYBOARD != 0 && self.keyboard.is_none() {
                let keyboard = self.new_id();
                self.send(Request::new(seat, request::SEAT_GET_KEYBOARD).uint(keyboard))?;
                self.keyboard = Some(keyboard);
            }
            return Ok(());
        }
        if Some(object) == self.pointer {
            return self.handle_pointer(found.opcode, &mut arguments);
        }
        if Some(object) == self.keyboard {
            return self.handle_keyboard(found.opcode, &mut arguments);
        }
        if found.opcode == event::BUFFER_RELEASE
            && let Some(buffer) = self.buffers.iter_mut().find(|buffer| buffer.id == object)
        {
            buffer.busy = false;
        }
        Ok(())
    }

    fn handle_pointer(&mut self, opcode: u16, arguments: &mut Arguments<'_>) -> Result<(), WindowError> {
        match opcode {
            event::POINTER_ENTER | event::POINTER_MOTION => {
                let _ = arguments.uint().map_err(protocol)?;
                if opcode == event::POINTER_ENTER {
                    let _ = arguments.uint().map_err(protocol)?;
                }
                let x = self.device(arguments.fixed().map_err(protocol)?);
                let y = self.device(arguments.fixed().map_err(protocol)?);
                self.pointer_at = (x, y);
                self.events.push_back(WindowEvent::PointerMoved { x, y });
            }
            event::POINTER_LEAVE => self.events.push_back(WindowEvent::PointerLeft),
            event::POINTER_BUTTON => {
                let _serial = arguments.uint().map_err(protocol)?;
                let _time = arguments.uint().map_err(protocol)?;
                let code = arguments.uint().map_err(protocol)?;
                let pressed = arguments.uint().map_err(protocol)? == 1;
                let button = match code {
                    wayland::BUTTON_LEFT => PointerButton::Primary,
                    wayland::BUTTON_RIGHT => PointerButton::Secondary,
                    wayland::BUTTON_MIDDLE => PointerButton::Middle,
                    _ => return Ok(()),
                };
                let (x, y) = self.pointer_at;
                let modifiers = self.modifiers;
                self.events.push_back(if pressed {
                    WindowEvent::PointerPressed { x, y, button, modifiers }
                } else {
                    WindowEvent::PointerReleased { x, y, button, modifiers }
                });
            }
            event::POINTER_AXIS => {
                let _time = arguments.uint().map_err(protocol)?;
                let axis = arguments.uint().map_err(protocol)?;
                let lines = arguments.fixed().map_err(protocol)? / WHEEL_UNITS_PER_LINE;
                let (x, y) = self.pointer_at;
                let (lines_down, lines_right) = if axis == 0 { (lines, 0.0) } else { (0.0, lines) };
                self.events.push_back(WindowEvent::Wheel {
                    x,
                    y,
                    lines_down,
                    lines_right,
                    modifiers: self.modifiers,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_keyboard(&mut self, opcode: u16, arguments: &mut Arguments<'_>) -> Result<(), WindowError> {
        match opcode {
            event::KEYBOARD_ENTER => self.events.push_back(WindowEvent::Focused(true)),
            event::KEYBOARD_LEAVE => {
                self.repeat.held = None;
                self.events.push_back(WindowEvent::Focused(false));
            }
            event::KEYBOARD_KEY => {
                let _serial = arguments.uint().map_err(protocol)?;
                let _time = arguments.uint().map_err(protocol)?;
                let code = arguments.uint().map_err(protocol)?;
                let pressed = arguments.uint().map_err(protocol)? == 1;
                let Some(press) = wayland::key_of(code, self.modifiers, self.caps_lock) else {
                    return Ok(());
                };
                if pressed {
                    self.events.push_back(WindowEvent::Key(press));
                    if self.repeat.rate > 0 {
                        self.repeat.held = Some((press, Instant::now() + self.repeat.delay));
                    }
                } else if self.repeat.held.is_some_and(|(held, _)| held.key == press.key) {
                    self.repeat.held = None;
                }
            }
            event::KEYBOARD_MODIFIERS => {
                let _serial = arguments.uint().map_err(protocol)?;
                let depressed = arguments.uint().map_err(protocol)?;
                let latched = arguments.uint().map_err(protocol)?;
                let locked = arguments.uint().map_err(protocol)?;
                self.modifiers = wayland::modifiers_of(depressed | latched);
                self.caps_lock = locked & 2 != 0;
            }
            event::KEYBOARD_REPEAT_INFO => {
                let rate = arguments.int().map_err(protocol)?;
                let delay = arguments.int().map_err(protocol)?;
                self.repeat.rate = u32::try_from(rate).unwrap_or(0);
                self.repeat.delay = Duration::from_millis(u64::try_from(delay).unwrap_or(0));
            }
            _ => {}
        }
        Ok(())
    }

    fn repeat_due(&mut self, now: Instant) -> Option<WindowEvent> {
        let (press, due) = self.repeat.held?;
        if now < due || self.repeat.rate == 0 {
            return None;
        }
        let every = Duration::from_secs(1) / self.repeat.rate;
        self.repeat.held = Some((press, now + every));
        Some(WindowEvent::Key(press))
    }

    fn new_pool(&mut self, bytes: u64) -> Result<(), WindowError> {
        for buffer in std::mem::take(&mut self.buffers) {
            self.send(Request::new(buffer.id, request::BUFFER_DESTROY))?;
        }
        if let Some((pool, _, _)) = self.pool.take() {
            self.send(Request::new(pool, request::POOL_DESTROY))?;
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        self.pool_number += 1;
        let path = runtime.join(format!("eggdb-wayland-{}-{}", std::process::id(), self.pool_number));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| io_error(&error))?;
        let _ = std::fs::remove_file(&path);
        file.set_len(bytes).map_err(|error| io_error(&error))?;
        let pool = self.new_id();
        let size = i32::try_from(bytes).map_err(|_| WindowError::InvalidInput("a frame too large for one shared-memory pool".to_owned()))?;
        let message = Request::new(self.shm, request::SHM_CREATE_POOL).uint(pool).int(size).finish();
        fd_passing::send_with_fd(self.stream.as_raw_fd(), &message, file.as_raw_fd()).map_err(|error| io_error(&error))?;
        self.pool = Some((pool, file, bytes));
        Ok(())
    }

    fn free_buffer(&mut self, width: u32, height: u32) -> Result<usize, WindowError> {
        let frame = u64::from(width) * u64::from(height) * 4;
        let fits = self.pool.as_ref().is_some_and(|(_, _, bytes)| *bytes >= frame * 2);
        if !fits {
            self.new_pool(frame * 2)?;
        }
        let stale: Vec<u32> = self
            .buffers
            .iter()
            .filter(|buffer| (buffer.width, buffer.height) != (width, height) && !buffer.busy)
            .map(|buffer| buffer.id)
            .collect();
        for id in stale {
            self.send(Request::new(id, request::BUFFER_DESTROY))?;
            self.buffers.retain(|buffer| buffer.id != id);
        }
        loop {
            if let Some(index) = self
                .buffers
                .iter()
                .position(|buffer| !buffer.busy && (buffer.width, buffer.height) == (width, height))
            {
                return Ok(index);
            }
            if self.buffers.len() < 2 {
                let taken: Vec<u64> = self.buffers.iter().map(|buffer| buffer.offset).collect();
                let offset = if taken.contains(&0) { frame } else { 0 };
                let id = self.new_id();
                let pool = self.pool.as_ref().map_or(0, |(pool, _, _)| *pool);
                let stride = i32::try_from(width * 4).unwrap_or(i32::MAX);
                self.send(
                    Request::new(pool, request::POOL_CREATE_BUFFER)
                        .uint(id)
                        .int(i32::try_from(offset).unwrap_or(0))
                        .int(i32::try_from(width).unwrap_or(i32::MAX))
                        .int(i32::try_from(height).unwrap_or(i32::MAX))
                        .int(stride)
                        .uint(wayland::FORMAT_ARGB8888),
                )?;
                self.buffers.push(ShmBuffer {
                    id,
                    offset,
                    width,
                    height,
                    busy: false,
                });
                continue;
            }
            self.read_events(None)?;
        }
    }
}

impl Window for WaylandWindow {
    fn size(&self) -> (u32, u32) {
        (self.logical.0 * self.scale, self.logical.1 * self.scale)
    }

    fn scale(&self) -> f32 {
        self.scale as f32
    }

    fn present(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), WindowError> {
        super::check_frame(width, height, rgba)?;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let index = self.free_buffer(width, height)?;
        let mut pixels = std::mem::take(&mut self.pixels);
        wayland::argb8888_from_rgba(rgba, &mut pixels);
        let buffer = self.buffers[index];
        let written = match &self.pool {
            Some((_, file, _)) => file.write_all_at(&pixels, buffer.offset),
            None => Ok(()),
        };
        self.pixels = pixels;
        written.map_err(|error| io_error(&error))?;
        let surface = self.surface;
        self.send(Request::new(surface, request::SURFACE_ATTACH).uint(buffer.id).int(0).int(0))?;
        self.send(
            Request::new(surface, request::SURFACE_DAMAGE_BUFFER)
                .int(0)
                .int(0)
                .int(i32::try_from(width).unwrap_or(i32::MAX))
                .int(i32::try_from(height).unwrap_or(i32::MAX)),
        )?;
        self.send(Request::new(surface, request::SURFACE_COMMIT))?;
        self.buffers[index].busy = true;
        Ok(())
    }

    fn next_event(&mut self, timeout: Option<Duration>) -> Result<Option<WindowEvent>, WindowError> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            if let Some(found) = self.events.pop_front() {
                return Ok(Some(found));
            }
            let now = Instant::now();
            if let Some(repeated) = self.repeat_due(now) {
                return Ok(Some(repeated));
            }
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(None);
            }
            let until_repeat = self.repeat.held.map(|(_, due)| due.saturating_duration_since(now));
            let until_deadline = deadline.map(|deadline| deadline.saturating_duration_since(now));
            let wait = match (until_repeat, until_deadline) {
                (Some(repeat), Some(deadline)) => Some(repeat.min(deadline)),
                (one, other) => one.or(other),
            };
            self.read_events(wait)?;
        }
    }
}
