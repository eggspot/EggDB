//! The Wayland protocol as EggDB's windows speak it (D1324), written from the public
//! specifications `wayland.xml` and `xdg-shell.xml`: the wire format's messages, the requests
//! and events a client window uses, the US keyboard map from Linux key codes, and pixels in
//! the compositor's `ARGB8888` order. Nothing here makes a system call, so it is tested on
//! every system.

use super::{Key, KeyPress, Modifiers};

/// The display object every connection starts with.
pub const DISPLAY_ID: u32 = 1;
/// `wl_shm`'s `argb8888` format.
pub const FORMAT_ARGB8888: u32 = 0;
/// `wl_seat`'s capability bit for a pointer.
pub const SEAT_POINTER: u32 = 1;
/// `wl_seat`'s capability bit for a keyboard.
pub const SEAT_KEYBOARD: u32 = 2;
/// Linux's code for the left mouse button.
pub const BUTTON_LEFT: u32 = 0x110;
/// Linux's code for the right mouse button.
pub const BUTTON_RIGHT: u32 = 0x111;
/// Linux's code for the middle mouse button.
pub const BUTTON_MIDDLE: u32 = 0x112;
/// The most bytes one message may take, as the protocol's 16-bit size field allows.
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// A request being written: its arguments in order, then [`Request::finish`] for the bytes.
#[derive(Debug)]
pub struct Request {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

impl Request {
    /// A request `opcode` on `object`.
    pub fn new(object: u32, opcode: u16) -> Self {
        Self { object, opcode, body: Vec::new() }
    }

    /// Adds an `uint`, `object` or `new_id` argument.
    pub fn uint(mut self, value: u32) -> Self {
        self.body.extend_from_slice(&value.to_ne_bytes());
        self
    }

    /// Adds an `int` argument.
    pub fn int(mut self, value: i32) -> Self {
        self.body.extend_from_slice(&value.to_ne_bytes());
        self
    }

    /// Adds a `string` argument: its length with the closing NUL, the bytes, the NUL, padding.
    pub fn string(mut self, text: &str) -> Self {
        let length = u32::try_from(text.len() + 1).unwrap_or(u32::MAX);
        self.body.extend_from_slice(&length.to_ne_bytes());
        self.body.extend_from_slice(text.as_bytes());
        self.body.push(0);
        while self.body.len() % 4 != 0 {
            self.body.push(0);
        }
        self
    }

    /// The message's bytes: object id, size and opcode, then the arguments.
    pub fn finish(self) -> Vec<u8> {
        let size = u32::try_from(8 + self.body.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(8 + self.body.len());
        out.extend_from_slice(&self.object.to_ne_bytes());
        out.extend_from_slice(&((size << 16) | u32::from(self.opcode)).to_ne_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

/// One event read from the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The object it is for.
    pub object: u32,
    /// Its opcode on that object's interface.
    pub opcode: u16,
    /// Its argument bytes.
    pub body: Vec<u8>,
}

/// Splits the next whole event off the front of `buffer`: `Ok(None)` while it holds less than
/// one, an error for a size the protocol does not allow.
pub fn next_event(buffer: &mut Vec<u8>) -> Result<Option<Event>, String> {
    if buffer.len() < 8 {
        return Ok(None);
    }
    let word = |at: usize| u32::from_ne_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]]);
    let object = word(0);
    let header = word(4);
    let size = (header >> 16) as usize;
    if !(8..=MAX_MESSAGE_BYTES).contains(&size) || size % 4 != 0 {
        return Err(format!("an event of {size} bytes"));
    }
    if buffer.len() < size {
        return Ok(None);
    }
    let body = buffer[8..size].to_vec();
    buffer.drain(..size);
    Ok(Some(Event {
        object,
        opcode: (header & 0xffff) as u16,
        body,
    }))
}

/// Reads an event's arguments in order.
#[derive(Debug)]
pub struct Arguments<'e> {
    body: &'e [u8],
    at: usize,
}

impl<'e> Arguments<'e> {
    /// The arguments of `event`.
    pub fn of(event: &'e Event) -> Self {
        Self { body: &event.body, at: 0 }
    }

    /// The next `uint`, `object` or `new_id`.
    pub fn uint(&mut self) -> Result<u32, String> {
        let bytes = self
            .body
            .get(self.at..self.at + 4)
            .ok_or_else(|| "an event ended before its arguments".to_owned())?;
        self.at += 4;
        Ok(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// The next `int`.
    pub fn int(&mut self) -> Result<i32, String> {
        self.uint().map(|value| value as i32)
    }

    /// The next `fixed`, a 24.8 fixed-point number.
    pub fn fixed(&mut self) -> Result<f32, String> {
        self.int().map(|value| value as f32 / 256.0)
    }

    /// The next `array`'s bytes.
    pub fn array(&mut self) -> Result<&'e [u8], String> {
        let length = self.uint()? as usize;
        let bytes = self
            .body
            .get(self.at..self.at + length)
            .ok_or_else(|| "an array longer than its event".to_owned())?;
        self.at += length.div_ceil(4) * 4;
        Ok(bytes)
    }

    /// The next `string`, without its closing NUL.
    pub fn string(&mut self) -> Result<String, String> {
        let bytes = self.array()?;
        let text = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        String::from_utf8(text.to_vec()).map_err(|_| "a string that is not UTF-8".to_owned())
    }
}

/// The keyboard's modifiers from `wl_keyboard.modifiers`' depressed and locked masks, by the
/// standard modifier order every XKB keymap uses (Shift, Lock, Control, Mod1 as Alt, Mod4 as
/// the Super key); Caps Lock is reported for letters by [`key_of`].
pub fn modifiers_of(depressed: u32) -> Modifiers {
    Modifiers {
        shift: depressed & 1 != 0,
        control: depressed & 4 != 0,
        alt: depressed & 8 != 0,
        command: false,
    }
}

const US_ROWS: [(u32, char, char); 47] = [
    (2, '1', '!'), (3, '2', '@'), (4, '3', '#'), (5, '4', '$'), (6, '5', '%'), (7, '6', '^'),
    (8, '7', '&'), (9, '8', '*'), (10, '9', '('), (11, '0', ')'), (12, '-', '_'), (13, '=', '+'),
    (16, 'q', 'Q'), (17, 'w', 'W'), (18, 'e', 'E'), (19, 'r', 'R'), (20, 't', 'T'), (21, 'y', 'Y'),
    (22, 'u', 'U'), (23, 'i', 'I'), (24, 'o', 'O'), (25, 'p', 'P'), (26, '[', '{'), (27, ']', '}'),
    (30, 'a', 'A'), (31, 's', 'S'), (32, 'd', 'D'), (33, 'f', 'F'), (34, 'g', 'G'), (35, 'h', 'H'),
    (36, 'j', 'J'), (37, 'k', 'K'), (38, 'l', 'L'), (39, ';', ':'), (40, '\'', '"'), (41, '`', '~'),
    (43, '\\', '|'), (44, 'z', 'Z'), (45, 'x', 'X'), (46, 'c', 'C'), (47, 'v', 'V'), (48, 'b', 'B'),
    (49, 'n', 'N'), (50, 'm', 'M'), (51, ',', '<'), (52, '.', '>'), (53, '/', '?'),
];

/// The key a Linux key code (`wl_keyboard.key`'s, without XKB's offset of 8) gives on a US
/// layout with `modifiers` held and Caps Lock `caps_lock`; `None` for modifier keys and keys the
/// GUI does not use.
pub fn key_of(code: u32, modifiers: Modifiers, caps_lock: bool) -> Option<KeyPress> {
    let key = match code {
        1 => Key::Escape,
        14 => Key::Backspace,
        15 => Key::Tab,
        28 | 96 => Key::Enter,
        57 => Key::Space,
        60 => Key::F2,
        62 => Key::F4,
        63 => Key::F5,
        68 => Key::F10,
        102 => Key::Home,
        103 => Key::Up,
        104 => Key::PageUp,
        105 => Key::Left,
        106 => Key::Right,
        107 => Key::End,
        108 => Key::Down,
        109 => Key::PageDown,
        111 => Key::Delete,
        127 => Key::ContextMenu,
        _ => {
            let &(_, plain, shifted) = US_ROWS.iter().find(|(row_code, _, _)| *row_code == code)?;
            let upper = if plain.is_ascii_alphabetic() {
                modifiers.shift != caps_lock
            } else {
                modifiers.shift
            };
            Key::Character(if upper { shifted } else { plain })
        }
    };
    Some(KeyPress { key, modifiers })
}

/// `rgba` (red, green, blue, alpha per pixel) in `wl_shm`'s `ARGB8888` order, which is blue,
/// green, red, alpha in memory on the little-endian machines Wayland runs on.
pub fn argb8888_from_rgba(rgba: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
}

/// Requests by interface, as numbered in the protocol files.
pub mod request {
    /// `wl_display.sync`.
    pub const DISPLAY_SYNC: u16 = 0;
    /// `wl_display.get_registry`.
    pub const DISPLAY_GET_REGISTRY: u16 = 1;
    /// `wl_registry.bind`.
    pub const REGISTRY_BIND: u16 = 0;
    /// `wl_compositor.create_surface`.
    pub const COMPOSITOR_CREATE_SURFACE: u16 = 0;
    /// `wl_shm.create_pool`.
    pub const SHM_CREATE_POOL: u16 = 0;
    /// `wl_shm_pool.create_buffer`.
    pub const POOL_CREATE_BUFFER: u16 = 0;
    /// `wl_shm_pool.destroy`.
    pub const POOL_DESTROY: u16 = 1;
    /// `wl_buffer.destroy`.
    pub const BUFFER_DESTROY: u16 = 0;
    /// `wl_surface.attach`.
    pub const SURFACE_ATTACH: u16 = 1;
    /// `wl_surface.commit`.
    pub const SURFACE_COMMIT: u16 = 6;
    /// `wl_surface.set_buffer_scale`.
    pub const SURFACE_SET_BUFFER_SCALE: u16 = 8;
    /// `wl_surface.damage_buffer`.
    pub const SURFACE_DAMAGE_BUFFER: u16 = 9;
    /// `xdg_wm_base.get_xdg_surface`.
    pub const WM_BASE_GET_XDG_SURFACE: u16 = 2;
    /// `xdg_wm_base.pong`.
    pub const WM_BASE_PONG: u16 = 3;
    /// `xdg_surface.get_toplevel`.
    pub const XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
    /// `xdg_surface.ack_configure`.
    pub const XDG_SURFACE_ACK_CONFIGURE: u16 = 4;
    /// `xdg_toplevel.set_title`.
    pub const TOPLEVEL_SET_TITLE: u16 = 2;
    /// `xdg_toplevel.set_app_id`.
    pub const TOPLEVEL_SET_APP_ID: u16 = 3;
    /// `xdg_toplevel.set_min_size`.
    pub const TOPLEVEL_SET_MIN_SIZE: u16 = 8;
    /// `wl_seat.get_pointer`.
    pub const SEAT_GET_POINTER: u16 = 0;
    /// `wl_seat.get_keyboard`.
    pub const SEAT_GET_KEYBOARD: u16 = 1;
}

/// Events by interface, as numbered in the protocol files.
pub mod event {
    /// `wl_display.error`.
    pub const DISPLAY_ERROR: u16 = 0;
    /// `wl_registry.global`.
    pub const REGISTRY_GLOBAL: u16 = 0;
    /// `wl_callback.done`.
    pub const CALLBACK_DONE: u16 = 0;
    /// `wl_buffer.release`.
    pub const BUFFER_RELEASE: u16 = 0;
    /// `xdg_wm_base.ping`.
    pub const WM_BASE_PING: u16 = 0;
    /// `xdg_surface.configure`.
    pub const XDG_SURFACE_CONFIGURE: u16 = 0;
    /// `xdg_toplevel.configure`.
    pub const TOPLEVEL_CONFIGURE: u16 = 0;
    /// `xdg_toplevel.close`.
    pub const TOPLEVEL_CLOSE: u16 = 1;
    /// `wl_seat.capabilities`.
    pub const SEAT_CAPABILITIES: u16 = 0;
    /// `wl_output.scale`.
    pub const OUTPUT_SCALE: u16 = 3;
    /// `wl_pointer.enter`.
    pub const POINTER_ENTER: u16 = 0;
    /// `wl_pointer.leave`.
    pub const POINTER_LEAVE: u16 = 1;
    /// `wl_pointer.motion`.
    pub const POINTER_MOTION: u16 = 2;
    /// `wl_pointer.button`.
    pub const POINTER_BUTTON: u16 = 3;
    /// `wl_pointer.axis`.
    pub const POINTER_AXIS: u16 = 4;
    /// `wl_keyboard.enter`.
    pub const KEYBOARD_ENTER: u16 = 1;
    /// `wl_keyboard.leave`.
    pub const KEYBOARD_LEAVE: u16 = 2;
    /// `wl_keyboard.key`.
    pub const KEYBOARD_KEY: u16 = 3;
    /// `wl_keyboard.modifiers`.
    pub const KEYBOARD_MODIFIERS: u16 = 4;
    /// `wl_keyboard.repeat_info`.
    pub const KEYBOARD_REPEAT_INFO: u16 = 5;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_writes_its_header_and_padded_string() {
        let bytes = Request::new(3, request::TOPLEVEL_SET_TITLE).string("EggDB").finish();
        assert_eq!(bytes.len(), 8 + 4 + 8);
        assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), (20 << 16) | 2);
        assert_eq!(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 6);
        assert_eq!(&bytes[12..20], b"EggDB\0\0\0");
    }

    #[test]
    fn events_split_off_whole_and_their_arguments_read_back() {
        let global = Request::new(2, event::REGISTRY_GLOBAL).uint(7).string("wl_shm").uint(1).finish();
        let mut buffer = global.clone();
        buffer.extend_from_slice(&global[..5]);
        let event = next_event(&mut buffer).unwrap().unwrap();
        assert_eq!((event.object, event.opcode), (2, 0));
        let mut arguments = Arguments::of(&event);
        assert_eq!(arguments.uint().unwrap(), 7);
        assert_eq!(arguments.string().unwrap(), "wl_shm");
        assert_eq!(arguments.uint().unwrap(), 1);
        assert!(arguments.uint().is_err());
        assert_eq!(next_event(&mut buffer).unwrap(), None);
        let mut hostile = vec![1, 0, 0, 0, 0, 0, 3, 0];
        assert!(next_event(&mut hostile).is_err());
    }

    #[test]
    fn fixed_numbers_and_pixels_convert() {
        let event = Event { object: 1, opcode: 0, body: (-384i32).to_ne_bytes().to_vec() };
        assert_eq!(Arguments::of(&event).fixed().unwrap(), -1.5);
        let mut out = Vec::new();
        argb8888_from_rgba(&[1, 2, 3, 4, 5, 6, 7, 8], &mut out);
        assert_eq!(out, [3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn keys_follow_the_us_layout_with_shift_and_caps_lock() {
        let plain = Modifiers::default();
        let shifted = modifiers_of(1);
        assert_eq!(key_of(30, plain, false).unwrap().key, Key::Character('a'));
        assert_eq!(key_of(30, shifted, false).unwrap().key, Key::Character('A'));
        assert_eq!(key_of(30, shifted, true).unwrap().key, Key::Character('a'));
        assert_eq!(key_of(2, plain, true).unwrap().key, Key::Character('1'));
        assert_eq!(key_of(2, shifted, false).unwrap().key, Key::Character('!'));
        assert_eq!(key_of(105, plain, false).unwrap().key, Key::Left);
        assert_eq!(key_of(42, shifted, false), None);
        let control = modifiers_of(4 | 8);
        assert!(control.control && control.alt && !control.shift);
    }
}
