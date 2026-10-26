use crate::terminal::{Input, TerminalSize};
use std::ffi::c_void;
use std::io::{self, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

type Handle = *mut c_void;

const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_WINDOW_INPUT: u32 = 0x0008;
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
const UTF8_CODE_PAGE: u32 = 65001;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const KEY_EVENT: u16 = 0x0001;
const WINDOW_BUFFER_SIZE_EVENT: u16 = 0x0004;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;
const RECORDS_PER_READ: usize = 32;
const LONGEST_WAIT: Duration = Duration::from_millis(100);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ScreenBufferInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: SmallRect,
    largest_window: Coord,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyEventRecord {
    key_down: i32,
    repeat_count: u16,
    virtual_key: u16,
    virtual_scan: u16,
    unicode_char: u16,
    control_keys: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputRecord {
    event_type: u16,
    padding: u16,
    key: KeyEventRecord,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(which: u32) -> Handle;
    fn GetConsoleMode(console: Handle, mode: *mut u32) -> i32;
    fn SetConsoleMode(console: Handle, mode: u32) -> i32;
    fn GetConsoleOutputCP() -> u32;
    fn SetConsoleOutputCP(code_page: u32) -> i32;
    fn GetConsoleScreenBufferInfo(console: Handle, info: *mut ScreenBufferInfo) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn ReadConsoleInputW(
        console: Handle,
        records: *mut InputRecord,
        length: u32,
        read: *mut u32,
    ) -> i32;
    fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SAVED_INPUT_MODE: AtomicU32 = AtomicU32::new(0);
static SAVED_OUTPUT_MODE: AtomicU32 = AtomicU32::new(0);
static SAVED_CODE_PAGE: AtomicU32 = AtomicU32::new(0);
static PANIC_HOOK: Once = Once::new();

#[derive(Debug)]
pub(crate) struct RawMode {
    input: Handle,
    output: Handle,
    pending: Vec<u8>,
    resized: bool,
    high_surrogate: Option<u16>,
}

fn standard_handle(which: u32) -> Handle {
    // SAFETY: GetStdHandle only returns the process's handle for that stream, or null.
    unsafe { GetStdHandle(which) }
}

fn console_mode(handle: Handle) -> Option<u32> {
    let mut mode = 0u32;
    // SAFETY: `mode` lives on this frame for the synchronous call; a handle that is not a
    // console makes the call fail.
    let ok = unsafe { GetConsoleMode(handle, &mut mode) };
    (ok != 0).then_some(mode)
}

fn set_console_mode(handle: Handle, mode: u32) -> bool {
    // SAFETY: the handle is this process's standard stream; the call only changes mode bits.
    unsafe { SetConsoleMode(handle, mode) != 0 }
}

pub(crate) fn both_streams_are_terminals() -> bool {
    console_mode(standard_handle(STD_INPUT_HANDLE)).is_some()
        && console_mode(standard_handle(STD_OUTPUT_HANDLE)).is_some()
}

impl RawMode {
    pub(crate) fn enter(start_sequence: &[u8]) -> io::Result<Self> {
        let input = standard_handle(STD_INPUT_HANDLE);
        let output = standard_handle(STD_OUTPUT_HANDLE);
        let (Some(input_mode), Some(output_mode)) = (console_mode(input), console_mode(output))
        else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "standard input and output are not both consoles",
            ));
        };
        if ACTIVE.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the console is already in raw mode",
            ));
        }
        let raw_output =
            (output_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING)
                | DISABLE_NEWLINE_AUTO_RETURN;
        if !set_console_mode(output, raw_output) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this console does not interpret escape sequences",
            ));
        }
        let raw_input = (input_mode
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT
            | ENABLE_WINDOW_INPUT;
        if !set_console_mode(input, raw_input) {
            set_console_mode(output, output_mode);
            return Err(io::Error::last_os_error());
        }
        // SAFETY: reads the console's output code page; no arguments.
        let code_page = unsafe { GetConsoleOutputCP() };
        SAVED_INPUT_MODE.store(input_mode, Ordering::Relaxed);
        SAVED_OUTPUT_MODE.store(output_mode, Ordering::Relaxed);
        SAVED_CODE_PAGE.store(code_page, Ordering::Relaxed);
        // SAFETY: sets the console's output code page to UTF-8; restored on exit.
        unsafe { SetConsoleOutputCP(UTF8_CODE_PAGE) };
        ACTIVE.store(true, Ordering::Release);
        // SAFETY: the handler is a plain function that only restores console modes.
        unsafe { SetConsoleCtrlHandler(Some(on_console_control), 1) };
        install_panic_hook();
        let mut mode = Self {
            input,
            output,
            pending: Vec::new(),
            resized: false,
            high_surrogate: None,
        };
        mode.write_all(start_sequence)?;
        Ok(mode)
    }

    pub(crate) fn size(&self) -> io::Result<TerminalSize> {
        let mut info = ScreenBufferInfo::default();
        // SAFETY: `info` lives on this frame and has the layout of CONSOLE_SCREEN_BUFFER_INFO.
        if unsafe { GetConsoleScreenBufferInfo(self.output, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let cols = i32::from(info.window.right) - i32::from(info.window.left) + 1;
        let rows = i32::from(info.window.bottom) - i32::from(info.window.top) + 1;
        Ok(TerminalSize {
            cols: u16::try_from(cols.max(0)).unwrap_or(u16::MAX),
            rows: u16::try_from(rows.max(0)).unwrap_or(u16::MAX),
        })
    }

    pub(crate) fn read_input(&mut self, buffer: &mut [u8], timeout: Duration) -> io::Result<Input> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if !self.pending.is_empty() {
                let count = buffer.len().min(self.pending.len());
                buffer[..count].copy_from_slice(&self.pending[..count]);
                self.pending.drain(..count);
                return Ok(Input::Bytes(count));
            }
            if std::mem::take(&mut self.resized) {
                return Ok(Input::Resized);
            }
            let left = deadline.map_or(LONGEST_WAIT, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            let slice = left.min(LONGEST_WAIT);
            let milliseconds = u32::try_from(slice.as_millis()).unwrap_or(u32::MAX);
            // SAFETY: waits on this process's console input handle for at most the slice.
            match unsafe { WaitForSingleObject(self.input, milliseconds) } {
                WAIT_OBJECT_0 => self.read_records()?,
                WAIT_TIMEOUT if left <= slice => return Ok(Input::TimedOut),
                WAIT_TIMEOUT => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }

    fn read_records(&mut self) -> io::Result<()> {
        let mut records = [InputRecord::default(); RECORDS_PER_READ];
        let mut read = 0u32;
        // SAFETY: the array and the count live on this frame; the length passed is the
        // array's.
        let ok = unsafe {
            ReadConsoleInputW(
                self.input,
                records.as_mut_ptr(),
                RECORDS_PER_READ as u32,
                &mut read,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let count = usize::try_from(read).unwrap_or(0).min(RECORDS_PER_READ);
        for record in &records[..count] {
            match record.event_type {
                KEY_EVENT if record.key.key_down != 0 && record.key.unicode_char != 0 => {
                    let repeat = record.key.repeat_count.max(1);
                    for _ in 0..repeat {
                        self.push_utf16(record.key.unicode_char);
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => self.resized = true,
                _ => {}
            }
        }
        Ok(())
    }

    fn push_utf16(&mut self, unit: u16) {
        let units: Vec<u16> = match self.high_surrogate.take() {
            Some(high) if (0xDC00..0xE000).contains(&unit) => vec![high, unit],
            _ if (0xD800..0xDC00).contains(&unit) => {
                self.high_surrogate = Some(unit);
                return;
            }
            _ => vec![unit],
        };
        for decoded in char::decode_utf16(units) {
            let character = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut encoded = [0u8; 4];
            self.pending
                .extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(bytes)?;
        stdout.flush()
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
        // SAFETY: removes the handler added in `enter`.
        unsafe { SetConsoleCtrlHandler(Some(on_console_control), 0) };
    }
}

fn restore() {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(crate::terminal::RESTORE_SEQUENCE);
    let _ = stdout.flush();
    set_console_mode(
        standard_handle(STD_INPUT_HANDLE),
        SAVED_INPUT_MODE.load(Ordering::Relaxed),
    );
    set_console_mode(
        standard_handle(STD_OUTPUT_HANDLE),
        SAVED_OUTPUT_MODE.load(Ordering::Relaxed),
    );
    // SAFETY: puts back the code page saved in `enter`.
    unsafe { SetConsoleOutputCP(SAVED_CODE_PAGE.load(Ordering::Relaxed)) };
}

extern "system" fn on_console_control(event: u32) -> i32 {
    if matches!(
        event,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    ) {
        restore();
    }
    0
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}
