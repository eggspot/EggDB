//! The parts of the X Window System protocol, version 11, that a window needs, encoded and
//! decoded from the public specification ("X Window System Protocol", X Consortium Standard):
//! the display name, `MIT-MAGIC-COOKIE-1` from an Xauthority file, connection setup, the
//! requests `CreateWindow`, `ChangeProperty`, `MapWindow`, `InternAtom`, `GetProperty`,
//! `CreateGC`, `PutImage`, `GetKeyboardMapping`, and for the `CLIPBOARD` selection
//! `SetSelectionOwner`, `ConvertSelection` and `SendEvent`, their replies, and the events a
//! window reads (keys, buttons, motion, selections). Every message is little-endian (the client chooses the byte order at setup). Parsing
//! never panics: a short or inconsistent message is a [`WindowError::Protocol`].

use super::{Key, Modifiers, PointerButton, WindowError};

/// The protocol's major version.
pub const PROTOCOL_MAJOR: u16 = 11;
/// The authorization protocol this client offers.
pub const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// The predefined atom `ATOM`.
pub const ATOM_ATOM: u32 = 4;
/// The predefined atom `CARDINAL`.
pub const ATOM_CARDINAL: u32 = 6;
/// The predefined atom `RESOURCE_MANAGER`, the root window's X resources.
pub const ATOM_RESOURCE_MANAGER: u32 = 23;
/// The predefined atom `STRING`.
pub const ATOM_STRING: u32 = 31;
/// The predefined atom `WM_NAME`.
pub const ATOM_WM_NAME: u32 = 39;
/// The predefined atom `WM_NORMAL_HINTS`.
pub const ATOM_WM_NORMAL_HINTS: u32 = 40;
/// The predefined atom `WM_SIZE_HINTS`.
pub const ATOM_WM_SIZE_HINTS: u32 = 41;

/// The event mask bit for `KeyPress`.
pub const EVENT_KEY_PRESS: u32 = 0x0000_0001;
/// The event mask bit for `Expose`.
pub const EVENT_EXPOSURE: u32 = 0x0000_8000;
/// The event mask bit for `StructureNotify` (`ConfigureNotify` among them).
pub const EVENT_STRUCTURE_NOTIFY: u32 = 0x0002_0000;
/// The event mask bit for `FocusIn` and `FocusOut`.
pub const EVENT_FOCUS_CHANGE: u32 = 0x0020_0000;
/// The event mask bit for `ButtonPress`.
pub const EVENT_BUTTON_PRESS: u32 = 0x0000_0004;
/// The event mask bit for `ButtonRelease`.
pub const EVENT_BUTTON_RELEASE: u32 = 0x0000_0008;
/// The event mask bit for `LeaveNotify`.
pub const EVENT_LEAVE_WINDOW: u32 = 0x0000_0020;
/// The event mask bit for `MotionNotify` with or without a button held.
pub const EVENT_POINTER_MOTION: u32 = 0x0000_0040;
/// The `Time` value `CurrentTime`.
pub const CURRENT_TIME: u32 = 0;
/// The atom value `None`, as in a refused `SelectionNotify`.
pub const ATOM_NONE: u32 = 0;
/// The code of a `SelectionNotify` event.
pub const EVENT_CODE_SELECTION_NOTIFY: u8 = 31;

const OPCODE_CREATE_WINDOW: u8 = 1;
const OPCODE_MAP_WINDOW: u8 = 8;
const OPCODE_INTERN_ATOM: u8 = 16;
const OPCODE_CHANGE_PROPERTY: u8 = 18;
const OPCODE_DELETE_PROPERTY: u8 = 19;
const OPCODE_GET_PROPERTY: u8 = 20;
const OPCODE_SET_SELECTION_OWNER: u8 = 22;
const OPCODE_CONVERT_SELECTION: u8 = 24;
const OPCODE_SEND_EVENT: u8 = 25;
const OPCODE_TRANSLATE_COORDINATES: u8 = 40;
const OPCODE_CREATE_GC: u8 = 55;
const OPCODE_PUT_IMAGE: u8 = 72;
const OPCODE_GET_KEYBOARD_MAPPING: u8 = 101;

const WINDOW_CLASS_INPUT_OUTPUT: u16 = 1;
const CW_BACK_PIXEL: u32 = 0x0000_0002;
const CW_EVENT_MASK: u32 = 0x0000_0800;
const IMAGE_FORMAT_Z_PIXMAP: u8 = 2;
const PUT_IMAGE_HEADER: usize = 24;
const SIZE_HINT_MIN_SIZE: u32 = 1 << 4;

const FAMILY_INTERNET: u16 = 0;
const FAMILY_LOCAL: u16 = 256;
const FAMILY_WILD: u16 = 65535;

fn protocol(message: impl Into<String>) -> WindowError {
    WindowError::Protocol(message.into())
}

fn pad4(length: usize) -> usize {
    (4 - length % 4) % 4
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, WindowError> {
    match bytes.get(at..at + 2) {
        Some(&[a, b]) => Ok(u16::from_le_bytes([a, b])),
        _ => Err(protocol(format!("the message ends before byte {}", at + 2))),
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, WindowError> {
    match bytes.get(at..at + 4) {
        Some(&[a, b, c, d]) => Ok(u32::from_le_bytes([a, b, c, d])),
        _ => Err(protocol(format!("the message ends before byte {}", at + 4))),
    }
}

fn u8_at(bytes: &[u8], at: usize) -> Result<u8, WindowError> {
    bytes
        .get(at)
        .copied()
        .ok_or_else(|| protocol(format!("the message ends before byte {}", at + 1)))
}

fn request(opcode: u8, data: u8, body: &[u8]) -> Result<Vec<u8>, WindowError> {
    let padded = body.len() + pad4(body.len());
    let units = u16::try_from((4 + padded) / 4).map_err(|_| {
        WindowError::InvalidInput("an X11 request is longer than 262140 bytes".into())
    })?;
    let mut bytes = Vec::with_capacity(4 + padded);
    bytes.push(opcode);
    bytes.push(data);
    bytes.extend_from_slice(&units.to_le_bytes());
    bytes.extend_from_slice(body);
    bytes.resize(4 + padded, 0);
    Ok(bytes)
}

/// A display named by `DISPLAY`: its number and default screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayName {
    /// The display number, `n` in `/tmp/.X11-unix/X<n>`.
    pub number: u32,
    /// The screen, 0 unless the name says `.<screen>`.
    pub screen: u32,
}

impl DisplayName {
    /// The Unix socket the display listens on.
    pub fn socket_path(&self) -> String {
        format!("/tmp/.X11-unix/X{}", self.number)
    }
}

/// Parses a `DISPLAY` value of the forms `:0`, `:0.1` and `unix:0`; a display reached over TCP
/// (`host:0`) is not supported.
pub fn parse_display(value: &str) -> Result<DisplayName, WindowError> {
    let Some((host, rest)) = value.rsplit_once(':') else {
        return Err(WindowError::NoDisplay(format!(
            "DISPLAY={value:?} names no display number"
        )));
    };
    if !host.is_empty() && host != "unix" {
        return Err(WindowError::Unsupported(
            "X11 displays over TCP are not supported; use a local display such as :0",
        ));
    }
    let (number, screen) = rest.split_once('.').unwrap_or((rest, "0"));
    let parse = |digits: &str| {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse::<u32>().ok()
    };
    match (parse(number), parse(screen)) {
        (Some(number), Some(screen)) => Ok(DisplayName { number, screen }),
        _ => Err(WindowError::NoDisplay(format!(
            "DISPLAY={value:?} is not of the form :<number>[.<screen>]"
        ))),
    }
}

/// An authorization the server asks for at setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    /// The authorization protocol's name.
    pub name: Vec<u8>,
    /// Its data: the 16-byte cookie.
    pub data: Vec<u8>,
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn big_endian_u16(&mut self) -> Option<u16> {
        let value = match self.bytes.get(self.at..self.at + 2)? {
            &[a, b] => u16::from_be_bytes([a, b]),
            _ => return None,
        };
        self.at += 2;
        Some(value)
    }

    fn counted(&mut self) -> Option<&'b [u8]> {
        let length = usize::from(self.big_endian_u16()?);
        let value = self.bytes.get(self.at..self.at + length)?;
        self.at += length;
        Some(value)
    }
}

/// Finds the `MIT-MAGIC-COOKIE-1` for local display `display` in an Xauthority file's bytes:
/// an entry for this host (`hostname`) first, then one for any host, then any other local
/// entry. A truncated file yields the best entry before the cut.
pub fn find_cookie(xauthority: &[u8], display: u32, hostname: Option<&[u8]>) -> Option<Cookie> {
    let number = display.to_string();
    let mut reader = Reader {
        bytes: xauthority,
        at: 0,
    };
    let mut best: Option<(u8, Cookie)> = None;
    while reader.at < xauthority.len() {
        let Some(family) = reader.big_endian_u16() else {
            break;
        };
        let (Some(address), Some(entry_number), Some(name), Some(data)) = (
            reader.counted(),
            reader.counted(),
            reader.counted(),
            reader.counted(),
        ) else {
            break;
        };
        if name != MIT_MAGIC_COOKIE
            || (!entry_number.is_empty() && entry_number != number.as_bytes())
        {
            continue;
        }
        let rank = match family {
            FAMILY_LOCAL if hostname == Some(address) => 3,
            FAMILY_WILD => 2,
            FAMILY_LOCAL | FAMILY_INTERNET => 1,
            _ => 0,
        };
        if rank > 0 && best.as_ref().is_none_or(|(held, _)| rank > *held) {
            best = Some((
                rank,
                Cookie {
                    name: name.to_vec(),
                    data: data.to_vec(),
                },
            ));
        }
    }
    best.map(|(_, cookie)| cookie)
}

/// The connection setup request: little-endian, protocol 11.0, with `cookie` when there is one.
pub fn encode_setup_request(cookie: Option<&Cookie>) -> Vec<u8> {
    let (name, data) = cookie.map_or((&[][..], &[][..]), |cookie| {
        (cookie.name.as_slice(), cookie.data.as_slice())
    });
    let name_length = u16::try_from(name.len()).unwrap_or(0);
    let data_length = u16::try_from(data.len()).unwrap_or(0);
    let mut bytes = vec![b'l', 0];
    bytes.extend_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&name_length.to_le_bytes());
    bytes.extend_from_slice(&data_length.to_le_bytes());
    bytes.extend_from_slice(&[0, 0]);
    if usize::from(name_length) == name.len() && usize::from(data_length) == data.len() {
        bytes.extend_from_slice(name);
        bytes.resize(bytes.len() + pad4(name.len()), 0);
        bytes.extend_from_slice(data);
        bytes.resize(bytes.len() + pad4(data.len()), 0);
    } else {
        bytes.truncate(6);
        bytes.extend_from_slice(&[0; 6]);
    }
    bytes
}

/// How many bytes follow the setup reply's first 8, from those 8.
pub fn setup_reply_remaining(header: &[u8; 8]) -> usize {
    usize::from(u16::from_le_bytes([header[6], header[7]])) * 4
}

/// A pixmap format the server supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixmapFormat {
    /// The depth.
    pub depth: u8,
    /// Bits per pixel in images of this depth.
    pub bits_per_pixel: u8,
    /// Each scanline is padded to a multiple of this many bits.
    pub scanline_pad: u8,
}

/// A visual type of a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visual {
    /// Its id.
    pub id: u32,
    /// The depth it is listed under.
    pub depth: u8,
    /// Its class (4 is `TrueColor`).
    pub class: u8,
    /// The bits of a pixel that hold red.
    pub red_mask: u32,
    /// The bits that hold green.
    pub green_mask: u32,
    /// The bits that hold blue.
    pub blue_mask: u32,
}

/// A screen of the display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// The root window.
    pub root: u32,
    /// The black pixel value.
    pub black_pixel: u32,
    /// The width in pixels.
    pub width: u16,
    /// The height in pixels.
    pub height: u16,
    /// The root window's visual.
    pub root_visual: u32,
    /// The root window's depth.
    pub root_depth: u8,
    /// Every visual of every allowed depth.
    pub visuals: Vec<Visual>,
}

/// What the server said at setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// The base of the resource ids this client may allocate.
    pub resource_id_base: u32,
    /// The bits of a resource id this client may vary.
    pub resource_id_mask: u32,
    /// The longest request in 4-byte units.
    pub maximum_request_length: u16,
    /// Whether images are most significant byte first.
    pub image_msb_first: bool,
    /// The smallest key code.
    pub min_keycode: u8,
    /// The largest key code.
    pub max_keycode: u8,
    /// The pixmap formats.
    pub formats: Vec<PixmapFormat>,
    /// The screens.
    pub screens: Vec<Screen>,
}

impl Setup {
    /// The `index`-th resource id this client may use, from 1, or `None` when the mask has no
    /// room for it.
    pub fn resource_id(&self, index: u32) -> Option<u32> {
        let shift = self.resource_id_mask.trailing_zeros();
        let value = index.checked_shl(shift)?;
        (value & self.resource_id_mask == value && value != 0)
            .then_some(self.resource_id_base | value)
    }
}

/// Parses the whole setup reply (its 8-byte header and what follows); a refusal becomes an
/// error carrying the server's reason.
pub fn parse_setup(bytes: &[u8]) -> Result<Setup, WindowError> {
    match u8_at(bytes, 0)? {
        1 => {}
        0 => {
            let length = usize::from(u8_at(bytes, 1)?);
            let reason = bytes.get(8..8 + length).unwrap_or_default();
            return Err(WindowError::NoDisplay(format!(
                "the X server refused the connection: {}",
                String::from_utf8_lossy(reason).trim()
            )));
        }
        2 => {
            let reason = bytes.get(8..).unwrap_or_default();
            return Err(WindowError::NoDisplay(format!(
                "the X server wants more authentication: {}",
                String::from_utf8_lossy(reason)
                    .trim_end_matches('\0')
                    .trim()
            )));
        }
        other => return Err(protocol(format!("setup answered with status {other}"))),
    }
    let vendor_length = usize::from(u16_at(bytes, 24)?);
    let screen_count = usize::from(u8_at(bytes, 28)?);
    let format_count = usize::from(u8_at(bytes, 29)?);
    let mut at = 40 + vendor_length + pad4(vendor_length);
    let mut formats = Vec::with_capacity(format_count);
    for _ in 0..format_count {
        formats.push(PixmapFormat {
            depth: u8_at(bytes, at)?,
            bits_per_pixel: u8_at(bytes, at + 1)?,
            scanline_pad: u8_at(bytes, at + 2)?,
        });
        at += 8;
    }
    let mut screens = Vec::with_capacity(screen_count.min(16));
    for _ in 0..screen_count {
        let depth_count = usize::from(u8_at(bytes, at + 39)?);
        let mut screen = Screen {
            root: u32_at(bytes, at)?,
            black_pixel: u32_at(bytes, at + 12)?,
            width: u16_at(bytes, at + 20)?,
            height: u16_at(bytes, at + 22)?,
            root_visual: u32_at(bytes, at + 32)?,
            root_depth: u8_at(bytes, at + 38)?,
            visuals: Vec::new(),
        };
        at += 40;
        for _ in 0..depth_count {
            let depth = u8_at(bytes, at)?;
            let visual_count = usize::from(u16_at(bytes, at + 2)?);
            at += 8;
            for _ in 0..visual_count {
                screen.visuals.push(Visual {
                    id: u32_at(bytes, at)?,
                    depth,
                    class: u8_at(bytes, at + 4)?,
                    red_mask: u32_at(bytes, at + 8)?,
                    green_mask: u32_at(bytes, at + 12)?,
                    blue_mask: u32_at(bytes, at + 16)?,
                });
                at += 24;
            }
        }
        screens.push(screen);
    }
    Ok(Setup {
        resource_id_base: u32_at(bytes, 12)?,
        resource_id_mask: u32_at(bytes, 16)?,
        maximum_request_length: u16_at(bytes, 26)?,
        image_msb_first: u8_at(bytes, 30)? == 1,
        min_keycode: u8_at(bytes, 34)?,
        max_keycode: u8_at(bytes, 35)?,
        formats,
        screens,
    })
}

/// Where red, green and blue go in a 32-bit pixel of a screen's visual, and the byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelLayout {
    /// The bit red's 8 bits start at.
    pub red_shift: u32,
    /// The bit green's 8 bits start at.
    pub green_shift: u32,
    /// The bit blue's 8 bits start at.
    pub blue_shift: u32,
    /// Whether pixels are written most significant byte first.
    pub msb_first: bool,
}

impl PixelLayout {
    /// The layout for drawing on `screen`'s root visual with `setup`'s formats: only 24- and
    /// 32-bit `TrueColor` visuals with 8 bits per channel in 32-bit pixels are supported.
    pub fn for_screen(setup: &Setup, screen: &Screen) -> Result<Self, WindowError> {
        let visual = screen
            .visuals
            .iter()
            .find(|visual| visual.id == screen.root_visual)
            .ok_or_else(|| protocol("the root visual is not among the screen's visuals"))?;
        let format = setup
            .formats
            .iter()
            .find(|format| format.depth == screen.root_depth)
            .ok_or_else(|| protocol("no pixmap format for the root depth"))?;
        let shift = |mask: u32| (mask.count_ones() == 8).then(|| mask.trailing_zeros());
        match (
            format.bits_per_pixel,
            shift(visual.red_mask),
            shift(visual.green_mask),
            shift(visual.blue_mask),
        ) {
            (32, Some(red_shift), Some(green_shift), Some(blue_shift)) => Ok(Self {
                red_shift,
                green_shift,
                blue_shift,
                msb_first: setup.image_msb_first,
            }),
            _ => Err(WindowError::Unsupported(
                "this X display's colour format is not supported (only 24-bit colour in 32-bit pixels)",
            )),
        }
    }

    /// `rgba` (four bytes a pixel) as this layout's pixels, four bytes each.
    pub fn encode(&self, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rgba.len());
        for &[red, green, blue, _] in rgba.as_chunks::<4>().0 {
            let (r, g, b) = (u32::from(red), u32::from(green), u32::from(blue));
            let value = (r << self.red_shift) | (g << self.green_shift) | (b << self.blue_shift);
            let bytes = if self.msb_first {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            };
            out.extend_from_slice(&bytes);
        }
        out
    }
}

/// `CreateWindow`: an input-output window `window` under `parent` of `width` × `height`
/// pixels, of the parent's depth and visual, filled with `background` and reporting the
/// `event_mask` events.
pub fn create_window(
    window: u32,
    parent: u32,
    width: u16,
    height: u16,
    background: u32,
    event_mask: u32,
) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(36);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&parent.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&WINDOW_CLASS_INPUT_OUTPUT.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&(CW_BACK_PIXEL | CW_EVENT_MASK).to_le_bytes());
    body.extend_from_slice(&background.to_le_bytes());
    body.extend_from_slice(&event_mask.to_le_bytes());
    request(OPCODE_CREATE_WINDOW, 0, &body)
}

/// `MapWindow`: shows `window`.
pub fn map_window(window: u32) -> Result<Vec<u8>, WindowError> {
    request(OPCODE_MAP_WINDOW, 0, &window.to_le_bytes())
}

/// `InternAtom`: the atom named `name`, created if it does not exist.
pub fn intern_atom(name: &str) -> Result<Vec<u8>, WindowError> {
    let length = u16::try_from(name.len())
        .map_err(|_| WindowError::InvalidInput("an atom name is longer than 65535 bytes".into()))?;
    let mut body = Vec::with_capacity(4 + name.len());
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(name.as_bytes());
    request(OPCODE_INTERN_ATOM, 0, &body)
}

/// The data of a property, in its format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyData<'d> {
    /// Format 8: bytes.
    Bytes(&'d [u8]),
    /// Format 32: 32-bit values.
    Words(&'d [u32]),
}

/// Whether an error the server sent about request `sequence` with `major_opcode` came from
/// answering another client's selection request: a `ChangeProperty` on its window or the
/// `SendEvent` that notifies it, among the requests in `serving`. That window may be gone
/// already, so the error is the other client's and must not end this one's event loop.
pub fn is_selection_serving_error(major_opcode: u8, sequence: u16, serving: &[u16]) -> bool {
    matches!(major_opcode, OPCODE_CHANGE_PROPERTY | OPCODE_SEND_EVENT)
        && serving.contains(&sequence)
}

/// `ChangeProperty` in mode `Replace`: sets `property` of `window` to `data` of type `kind`.
pub fn change_property(
    window: u32,
    property: u32,
    kind: u32,
    data: PropertyData<'_>,
) -> Result<Vec<u8>, WindowError> {
    let (format, units, bytes) = match data {
        PropertyData::Bytes(bytes) => (8u8, bytes.len(), bytes.to_vec()),
        PropertyData::Words(words) => (
            32u8,
            words.len(),
            words.iter().flat_map(|word| word.to_le_bytes()).collect(),
        ),
    };
    let units = u32::try_from(units)
        .map_err(|_| WindowError::InvalidInput("a property is too long".into()))?;
    let mut body = Vec::with_capacity(20 + bytes.len());
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&kind.to_le_bytes());
    body.push(format);
    body.extend_from_slice(&[0, 0, 0]);
    body.extend_from_slice(&units.to_le_bytes());
    body.extend_from_slice(&bytes);
    request(OPCODE_CHANGE_PROPERTY, 0, &body)
}

/// The `WM_SIZE_HINTS` value asking for a minimum size of `width` × `height` pixels.
pub fn minimum_size_hints(width: u32, height: u32) -> [u32; 18] {
    let mut hints = [0u32; 18];
    hints[0] = SIZE_HINT_MIN_SIZE;
    hints[5] = width;
    hints[6] = height;
    hints
}

/// `GetProperty`: up to `max_units` 4-byte units of `property` of `window`, of any type.
pub fn get_property(window: u32, property: u32, max_units: u32) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&max_units.to_le_bytes());
    request(OPCODE_GET_PROPERTY, 0, &body)
}

/// `GetProperty` with `delete` set: like [`get_property`], and the server deletes the
/// property once all of it was read, as a selection's requestor does.
pub fn get_property_deleting(
    window: u32,
    property: u32,
    max_units: u32,
) -> Result<Vec<u8>, WindowError> {
    let mut bytes = get_property(window, property, max_units)?;
    if let Some(delete) = bytes.get_mut(1) {
        *delete = 1;
    }
    Ok(bytes)
}

/// `DeleteProperty`: removes `property` from `window`.
pub fn delete_property(window: u32, property: u32) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&property.to_le_bytes());
    request(OPCODE_DELETE_PROPERTY, 0, &body)
}

/// `SetSelectionOwner`: makes `owner` the owner of `selection` (`None` gives it up).
pub fn set_selection_owner(owner: u32, selection: u32, time: u32) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&owner.to_le_bytes());
    body.extend_from_slice(&selection.to_le_bytes());
    body.extend_from_slice(&time.to_le_bytes());
    request(OPCODE_SET_SELECTION_OWNER, 0, &body)
}

/// `ConvertSelection`: asks `selection`'s owner to put it, as `target`, in `property` of
/// `requestor` and answer with a `SelectionNotify`.
pub fn convert_selection(
    requestor: u32,
    selection: u32,
    target: u32,
    property: u32,
    time: u32,
) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(20);
    for value in [requestor, selection, target, property, time] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    request(OPCODE_CONVERT_SELECTION, 0, &body)
}

/// A `SelectionNotify` event telling `requestor` that `selection` is in `property` as
/// `target`, or, with `property` [`ATOM_NONE`], that it could not be converted.
pub fn selection_notify_event(
    time: u32,
    requestor: u32,
    selection: u32,
    target: u32,
    property: u32,
) -> [u8; 32] {
    let mut event = [0u8; 32];
    event[0] = EVENT_CODE_SELECTION_NOTIFY;
    for (at, value) in [
        (4, time),
        (8, requestor),
        (12, selection),
        (16, target),
        (20, property),
    ] {
        event[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    event
}

/// `SendEvent` with no propagation and an empty event mask: delivers `event` to the client
/// that created `destination`.
pub fn send_event(destination: u32, event: &[u8; 32]) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(40);
    body.extend_from_slice(&destination.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(event);
    request(OPCODE_SEND_EVENT, 0, &body)
}

/// The type atom of a `GetProperty` reply (`None` when the property does not exist).
pub fn parse_property_type(reply: &[u8]) -> Result<u32, WindowError> {
    if u8_at(reply, 0)? != 1 {
        return Err(protocol("a GetProperty answer is not a reply"));
    }
    u32_at(reply, 8)
}

/// What a pointer button's code means: one of the three buttons, a wheel step, or a button
/// the GUI does not use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ButtonCode {
    /// Button 1, 2 or 3.
    Button(PointerButton),
    /// Buttons 4 to 7: one wheel step up, down, left or right, in lines.
    Wheel {
        /// Lines down (negative: up).
        lines_down: f32,
        /// Lines right (negative: left).
        lines_right: f32,
    },
    /// Any other button.
    Other,
}

/// The meaning of button code `detail` in a `ButtonPress` or `ButtonRelease`: 1 primary,
/// 2 middle, 3 secondary, and 4 to 7 the wheel's steps up, down, left and right.
pub fn button_code(detail: u8) -> ButtonCode {
    match detail {
        1 => ButtonCode::Button(PointerButton::Primary),
        2 => ButtonCode::Button(PointerButton::Middle),
        3 => ButtonCode::Button(PointerButton::Secondary),
        4 => ButtonCode::Wheel {
            lines_down: -1.0,
            lines_right: 0.0,
        },
        5 => ButtonCode::Wheel {
            lines_down: 1.0,
            lines_right: 0.0,
        },
        6 => ButtonCode::Wheel {
            lines_down: 0.0,
            lines_right: -1.0,
        },
        7 => ButtonCode::Wheel {
            lines_down: 0.0,
            lines_right: 1.0,
        },
        _ => ButtonCode::Other,
    }
}

/// `CreateGC`: a graphics context `gc` for `drawable` with every value at its default.
pub fn create_gc(gc: u32, drawable: u32) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&gc.to_le_bytes());
    body.extend_from_slice(&drawable.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    request(OPCODE_CREATE_GC, 0, &body)
}

/// Where a `PutImage` puts its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageTarget {
    /// The window or pixmap.
    pub drawable: u32,
    /// The graphics context.
    pub gc: u32,
    /// The drawable's depth.
    pub depth: u8,
}

/// `PutImage` in `ZPixmap` format: `pixels` (already in the screen's layout, four bytes a
/// pixel) as a `width` × `height` image at (`x`, `y`).
pub fn put_image(
    target: ImageTarget,
    width: u16,
    height: u16,
    x: i16,
    y: i16,
    pixels: &[u8],
) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(20 + pixels.len());
    body.extend_from_slice(&target.drawable.to_le_bytes());
    body.extend_from_slice(&target.gc.to_le_bytes());
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    body.push(0);
    body.push(target.depth);
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(pixels);
    request(OPCODE_PUT_IMAGE, IMAGE_FORMAT_Z_PIXMAP, &body)
}

/// How many rows of a `width`-pixel, 32-bit image fit in one `PutImage` under a maximum
/// request length of `maximum_units` 4-byte units; `None` when not even one row fits.
pub fn rows_per_put_image(width: u32, maximum_units: u16) -> Option<u32> {
    let limit = usize::from(maximum_units) * 4;
    let row = usize::try_from(width).ok()?.checked_mul(4)?;
    if row == 0 {
        return None;
    }
    let rows = limit.checked_sub(PUT_IMAGE_HEADER)? / row;
    u32::try_from(rows.min(usize::from(u16::MAX)))
        .ok()
        .filter(|rows| *rows > 0)
}

/// `GetKeyboardMapping`: the key symbols of `count` key codes from `first`.
pub fn get_keyboard_mapping(first: u8, count: u8) -> Result<Vec<u8>, WindowError> {
    request(OPCODE_GET_KEYBOARD_MAPPING, 0, &[first, count, 0, 0])
}

/// `TranslateCoordinates`: where the top left of `window` is in `destination`, usually the
/// root window, so the window's place on the screen is known.
pub fn translate_coordinates(window: u32, destination: u32) -> Result<Vec<u8>, WindowError> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&destination.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    request(OPCODE_TRANSLATE_COORDINATES, 0, &body)
}

/// The point in a `TranslateCoordinates` reply, in the destination window's pixels.
pub fn parse_translate_coordinates_reply(reply: &[u8]) -> Result<(i32, i32), WindowError> {
    if u8_at(reply, 0)? != 1 {
        return Err(protocol("a TranslateCoordinates answer is not a reply"));
    }
    let x = u16_at(reply, 12)?.cast_signed();
    let y = u16_at(reply, 14)?.cast_signed();
    Ok((i32::from(x), i32::from(y)))
}

/// How many bytes follow a 32-byte packet's first 32: a reply's extra length, 0 otherwise.
pub fn packet_remaining(header: &[u8; 32]) -> usize {
    if header[0] == 1 {
        (u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize)
            .saturating_mul(4)
    } else {
        0
    }
}

/// The sequence number of a reply or error, or of the last request an event follows.
pub fn packet_sequence(header: &[u8; 32]) -> u16 {
    u16::from_le_bytes([header[2], header[3]])
}

/// The atom in an `InternAtom` reply.
pub fn parse_intern_atom_reply(reply: &[u8]) -> Result<u32, WindowError> {
    if u8_at(reply, 0)? != 1 {
        return Err(protocol("an InternAtom answer is not a reply"));
    }
    match u32_at(reply, 8)? {
        0 => Err(protocol("InternAtom returned no atom")),
        atom => Ok(atom),
    }
}

/// The value bytes of a `GetProperty` reply (empty when the property does not exist).
pub fn parse_property_reply(reply: &[u8]) -> Result<&[u8], WindowError> {
    if u8_at(reply, 0)? != 1 {
        return Err(protocol("a GetProperty answer is not a reply"));
    }
    let format = u8_at(reply, 1)?;
    let units = u32_at(reply, 16)? as usize;
    let length = match format {
        0 => 0,
        8 => units,
        16 => units.saturating_mul(2),
        32 => units.saturating_mul(4),
        other => return Err(protocol(format!("a property of format {other}"))),
    };
    reply
        .get(32..32usize.saturating_add(length))
        .ok_or_else(|| protocol("a GetProperty reply is shorter than its value"))
}

/// The key symbols of every key code, as `GetKeyboardMapping` returned them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyboardMapping {
    /// The first key code described.
    pub first_keycode: u8,
    /// How many symbols each key code has.
    pub per_keycode: u8,
    /// The symbols, `per_keycode` for each key code from the first.
    pub keysyms: Vec<u32>,
}

impl KeyboardMapping {
    /// The symbol `keycode` types, its shifted one when `shift` is held and it has one.
    pub fn keysym(&self, keycode: u8, shift: bool) -> Option<u32> {
        let per = usize::from(self.per_keycode);
        let index = usize::from(keycode.checked_sub(self.first_keycode)?).checked_mul(per)?;
        let symbols = self.keysyms.get(index..index.checked_add(per)?)?;
        let base = symbols.first().copied().filter(|&symbol| symbol != 0)?;
        let shifted = symbols.get(1).copied().filter(|&symbol| symbol != 0);
        if !shift {
            return Some(base);
        }
        Some(shifted.unwrap_or_else(|| upper_case_keysym(base)))
    }
}

fn upper_case_keysym(keysym: u32) -> u32 {
    match keysym {
        0x61..=0x7A => keysym - 0x20,
        _ => keysym,
    }
}

/// Parses a `GetKeyboardMapping` reply for a request that started at `first_keycode`.
pub fn parse_keyboard_mapping_reply(
    reply: &[u8],
    first_keycode: u8,
) -> Result<KeyboardMapping, WindowError> {
    if u8_at(reply, 0)? != 1 {
        return Err(protocol("a GetKeyboardMapping answer is not a reply"));
    }
    let per_keycode = u8_at(reply, 1)?;
    let count = u32_at(reply, 4)? as usize;
    let mut keysyms = Vec::with_capacity(count.min(1 << 16));
    for index in 0..count {
        keysyms.push(u32_at(reply, 32 + index * 4)?);
    }
    Ok(KeyboardMapping {
        first_keycode,
        per_keycode,
        keysyms,
    })
}

/// A packet the server sent: an error, a reply or an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet {
    /// The server refused a request.
    Error {
        /// The error code (for example 16 `BadLength`).
        code: u8,
        /// The refused request's sequence number.
        sequence: u16,
        /// Its major opcode.
        major_opcode: u8,
    },
    /// A reply to the request with this sequence number.
    Reply {
        /// The request's sequence number.
        sequence: u16,
    },
    /// A key was pressed.
    KeyPress {
        /// The key code.
        keycode: u8,
        /// The modifier and button state.
        state: u16,
    },
    /// Part of a window must be drawn again; `count` more `Expose` events follow.
    Expose {
        /// The window.
        window: u32,
        /// How many more follow.
        count: u16,
    },
    /// A window's size or place changed.
    ConfigureNotify {
        /// The window.
        window: u32,
        /// Its width.
        width: u16,
        /// Its height.
        height: u16,
    },
    /// A message from another client, such as the window manager's `WM_DELETE_WINDOW`.
    ClientMessage {
        /// The window.
        window: u32,
        /// The message's type atom.
        kind: u32,
        /// Its first 32-bit value.
        first: u32,
    },
    /// A pointer button went down in the window.
    ButtonPress {
        /// The button's code (see [`button_code`]).
        detail: u8,
        /// The pointer's position in the window.
        x: i16,
        /// The pointer's vertical position in the window.
        y: i16,
        /// The modifier and button state.
        state: u16,
    },
    /// A pointer button went up.
    ButtonRelease {
        /// The button's code.
        detail: u8,
        /// The pointer's position in the window.
        x: i16,
        /// The pointer's vertical position in the window.
        y: i16,
        /// The modifier and button state.
        state: u16,
    },
    /// The pointer moved.
    MotionNotify {
        /// The pointer's position in the window.
        x: i16,
        /// The pointer's vertical position in the window.
        y: i16,
    },
    /// The pointer left a window.
    LeaveNotify {
        /// The window it left.
        window: u32,
        /// 0 for a normal leave, 1 and 2 for leaves caused by grabs.
        mode: u8,
    },
    /// Another client took a selection this one owned.
    SelectionClear {
        /// The window that owned it.
        owner: u32,
        /// The selection's atom.
        selection: u32,
    },
    /// Another client asks for a selection this one owns.
    SelectionRequest {
        /// The request's time.
        time: u32,
        /// The owner window.
        owner: u32,
        /// The window to answer.
        requestor: u32,
        /// The selection's atom.
        selection: u32,
        /// The type asked for, such as `UTF8_STRING` or `TARGETS`.
        target: u32,
        /// The requestor's property to write it into (`None` from obsolete clients).
        property: u32,
    },
    /// The answer to this client's `ConvertSelection`.
    SelectionNotify {
        /// The window that asked.
        requestor: u32,
        /// The selection's atom.
        selection: u32,
        /// The type it was converted to.
        target: u32,
        /// The property holding it, `None` when it could not be converted.
        property: u32,
    },
    /// The window got the keyboard focus.
    FocusIn,
    /// The window lost the keyboard focus.
    FocusOut,
    /// The keyboard mapping changed and must be read again.
    KeyboardMappingChanged,
    /// Any other event, by code.
    Other(u8),
}

/// Parses a 32-byte packet's header.
pub fn parse_packet(header: &[u8; 32]) -> Packet {
    let word = |at: usize| {
        u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let half = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    match header[0] & 0x7F {
        0 => Packet::Error {
            code: header[1],
            sequence: half(2),
            major_opcode: header[10],
        },
        1 => Packet::Reply { sequence: half(2) },
        2 => Packet::KeyPress {
            keycode: header[1],
            state: half(28),
        },
        4 | 5 => {
            let (detail, x, y, state) = (header[1], half(24) as i16, half(26) as i16, half(28));
            if header[0] & 0x7F == 4 {
                Packet::ButtonPress {
                    detail,
                    x,
                    y,
                    state,
                }
            } else {
                Packet::ButtonRelease {
                    detail,
                    x,
                    y,
                    state,
                }
            }
        }
        6 => Packet::MotionNotify {
            x: half(24) as i16,
            y: half(26) as i16,
        },
        8 => Packet::LeaveNotify {
            window: word(12),
            mode: header[30],
        },
        29 => Packet::SelectionClear {
            owner: word(8),
            selection: word(12),
        },
        30 => Packet::SelectionRequest {
            time: word(4),
            owner: word(8),
            requestor: word(12),
            selection: word(16),
            target: word(20),
            property: word(24),
        },
        31 => Packet::SelectionNotify {
            requestor: word(8),
            selection: word(12),
            target: word(16),
            property: word(20),
        },
        9 => Packet::FocusIn,
        10 => Packet::FocusOut,
        12 => Packet::Expose {
            window: word(4),
            count: half(16),
        },
        22 => Packet::ConfigureNotify {
            window: word(8),
            width: half(20),
            height: half(22),
        },
        33 => Packet::ClientMessage {
            window: word(4),
            kind: word(8),
            first: word(12),
        },
        34 if header[4] == 1 => Packet::KeyboardMappingChanged,
        code => Packet::Other(code),
    }
}

/// The modifiers in a key event's state: Shift, Control and Mod1 (Alt).
pub fn modifiers_from_state(state: u16) -> Modifiers {
    Modifiers {
        shift: state & 0x0001 != 0,
        control: state & 0x0004 != 0,
        alt: state & 0x0008 != 0,
        command: false,
    }
}

/// The key a key symbol stands for, if the GUI uses it.
pub fn key_from_keysym(keysym: u32) -> Option<Key> {
    Some(match keysym {
        0xFF09 | 0xFE20 => Key::Tab,
        0xFF0D | 0xFF8D => Key::Enter,
        0xFF1B => Key::Escape,
        0x20 => Key::Space,
        0xFF08 => Key::Backspace,
        0xFFFF => Key::Delete,
        0xFF51 => Key::Left,
        0xFF52 => Key::Up,
        0xFF53 => Key::Right,
        0xFF54 => Key::Down,
        0xFF50 => Key::Home,
        0xFF57 => Key::End,
        0xFFC1 => Key::F4,
        0xFFBF => Key::F2,
        0xFFC2 => Key::F5,
        0xFF55 => Key::PageUp,
        0xFF56 => Key::PageDown,
        0x21..=0x7E | 0xA0..=0xFF => Key::Character(char::from_u32(keysym)?),
        0x0100_0000..=0x0110_FFFF => Key::Character(char::from_u32(keysym - 0x0100_0000)?),
        _ => return None,
    })
}

/// The display scale from the root window's X resources: `Xft.dpi` divided by 96.
pub fn scale_from_resources(resources: &[u8]) -> Option<f32> {
    let text = std::str::from_utf8(resources).ok()?;
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != "Xft.dpi" {
            return None;
        }
        let dpi: f32 = value.trim().parse().ok()?;
        (dpi.is_finite() && (48.0..=960.0).contains(&dpi)).then_some(dpi / 96.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_selection_serving_errors_are_ignored() {
        let serving = [7, 9];
        assert!(is_selection_serving_error(
            OPCODE_CHANGE_PROPERTY,
            7,
            &serving
        ));
        assert!(is_selection_serving_error(OPCODE_SEND_EVENT, 9, &serving));
        assert!(!is_selection_serving_error(
            OPCODE_CHANGE_PROPERTY,
            8,
            &serving
        ));
        assert!(!is_selection_serving_error(
            OPCODE_GET_PROPERTY,
            7,
            &serving
        ));
        assert!(!is_selection_serving_error(OPCODE_SEND_EVENT, 9, &[]));
    }

    #[test]
    fn display_names_parse_to_a_socket_and_tcp_is_refused() {
        assert_eq!(
            parse_display(":0"),
            Ok(DisplayName {
                number: 0,
                screen: 0
            })
        );
        assert_eq!(
            parse_display("unix:12.1"),
            Ok(DisplayName {
                number: 12,
                screen: 1
            })
        );
        assert_eq!(
            parse_display(":3").map(|name| name.socket_path()),
            Ok("/tmp/.X11-unix/X3".to_owned())
        );
        assert!(matches!(
            parse_display("example.com:0"),
            Err(WindowError::Unsupported(_))
        ));
        for bad in ["", "0", ":", ":x", ":1.", ":-1", ":99999999999"] {
            assert!(parse_display(bad).is_err(), "{bad:?}");
        }
    }

    fn xauth_entry(
        family: u16,
        address: &[u8],
        number: &[u8],
        name: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        let mut bytes = family.to_be_bytes().to_vec();
        for field in [address, number, name, data] {
            bytes.extend_from_slice(&(field.len() as u16).to_be_bytes());
            bytes.extend_from_slice(field);
        }
        bytes
    }

    #[test]
    fn the_cookie_for_this_host_and_display_is_found_in_an_xauthority_file() {
        let mut file = xauth_entry(FAMILY_LOCAL, b"other", b"0", MIT_MAGIC_COOKIE, &[1; 16]);
        file.extend(xauth_entry(
            FAMILY_LOCAL,
            b"box",
            b"1",
            MIT_MAGIC_COOKIE,
            &[2; 16],
        ));
        file.extend(xauth_entry(
            FAMILY_LOCAL,
            b"box",
            b"0",
            b"XDM-AUTHORIZATION-1",
            &[3; 16],
        ));
        file.extend(xauth_entry(
            FAMILY_LOCAL,
            b"box",
            b"0",
            MIT_MAGIC_COOKIE,
            &[4; 16],
        ));
        let found = find_cookie(&file, 0, Some(b"box")).map(|cookie| cookie.data);
        assert_eq!(found, Some(vec![4; 16]));
        let fallback = find_cookie(&file, 0, None).map(|cookie| cookie.data);
        assert_eq!(fallback, Some(vec![1; 16]));
        assert_eq!(find_cookie(&file, 7, Some(b"box")), None);
        let mut truncated = file.clone();
        truncated.truncate(file.len() - 3);
        assert_eq!(
            find_cookie(&truncated, 0, Some(b"box")).map(|cookie| cookie.data),
            Some(vec![1; 16])
        );
        assert_eq!(find_cookie(&[0xFF], 0, None), None);
    }

    #[test]
    fn the_setup_request_is_encoded_as_the_spec_lays_it_out() {
        assert_eq!(
            encode_setup_request(None),
            vec![b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        let cookie = Cookie {
            name: MIT_MAGIC_COOKIE.to_vec(),
            data: vec![0xAB; 16],
        };
        let bytes = encode_setup_request(Some(&cookie));
        assert_eq!(&bytes[..12], &[b'l', 0, 11, 0, 0, 0, 18, 0, 16, 0, 0, 0]);
        assert_eq!(&bytes[12..30], MIT_MAGIC_COOKIE);
        assert_eq!(&bytes[30..32], &[0, 0]);
        assert_eq!(&bytes[32..48], &[0xAB; 16]);
        assert_eq!(bytes.len(), 48);
    }

    fn setup_reply() -> Vec<u8> {
        let vendor = b"EggX";
        let mut extra = Vec::new();
        extra.extend_from_slice(&0u32.to_le_bytes());
        extra.extend_from_slice(&0x0040_0000u32.to_le_bytes());
        extra.extend_from_slice(&0x001F_FFFFu32.to_le_bytes());
        extra.extend_from_slice(&0u32.to_le_bytes());
        extra.extend_from_slice(&(vendor.len() as u16).to_le_bytes());
        extra.extend_from_slice(&0xFFFFu16.to_le_bytes());
        extra.extend_from_slice(&[1, 2, 0, 0, 32, 32, 8, 255]);
        extra.extend_from_slice(&[0; 4]);
        extra.extend_from_slice(vendor);
        extra.extend_from_slice(&[1, 1, 32, 0, 0, 0, 0, 0]);
        extra.extend_from_slice(&[24, 32, 32, 0, 0, 0, 0, 0]);
        extra.extend_from_slice(&0x0000_0123u32.to_le_bytes());
        extra.extend_from_slice(&0x20u32.to_le_bytes());
        extra.extend_from_slice(&0x00FF_FFFFu32.to_le_bytes());
        extra.extend_from_slice(&0u32.to_le_bytes());
        extra.extend_from_slice(&0u32.to_le_bytes());
        extra.extend_from_slice(&1920u16.to_le_bytes());
        extra.extend_from_slice(&1080u16.to_le_bytes());
        extra.extend_from_slice(&[0; 8]);
        extra.extend_from_slice(&0x21u32.to_le_bytes());
        extra.extend_from_slice(&[0, 0, 24, 1]);
        extra.extend_from_slice(&[24, 0, 1, 0, 0, 0, 0, 0]);
        extra.extend_from_slice(&0x21u32.to_le_bytes());
        extra.extend_from_slice(&[4, 8, 0, 1]);
        extra.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
        extra.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
        extra.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
        extra.extend_from_slice(&[0; 4]);
        let mut reply = vec![1, 0, 11, 0, 0, 0];
        reply.extend_from_slice(&((extra.len() / 4) as u16).to_le_bytes());
        reply.extend(extra);
        reply
    }

    #[test]
    fn a_setup_reply_yields_ids_formats_screens_and_the_pixel_layout() {
        let reply = setup_reply();
        let header: [u8; 8] = reply[..8].try_into().unwrap();
        assert_eq!(setup_reply_remaining(&header), reply.len() - 8);
        let setup = parse_setup(&reply).unwrap();
        assert_eq!(setup.resource_id_base, 0x0040_0000);
        assert_eq!(setup.resource_id(1), Some(0x0040_0001));
        assert_eq!(setup.resource_id(0x0020_0000), None);
        assert_eq!(setup.maximum_request_length, 0xFFFF);
        assert_eq!((setup.min_keycode, setup.max_keycode), (8, 255));
        assert!(!setup.image_msb_first);
        let screen = &setup.screens[0];
        assert_eq!(
            (screen.root, screen.width, screen.height),
            (0x123, 1920, 1080)
        );
        assert_eq!((screen.root_visual, screen.root_depth), (0x21, 24));
        let layout = PixelLayout::for_screen(&setup, screen).unwrap();
        assert_eq!(
            (layout.red_shift, layout.green_shift, layout.blue_shift),
            (16, 8, 0)
        );
        assert_eq!(
            layout.encode(&[0x11, 0x22, 0x33, 0xFF]),
            vec![0x33, 0x22, 0x11, 0]
        );
        for cut in 0..reply.len() {
            let _ = parse_setup(&reply[..cut]);
        }
        let mut refused = vec![0, 13, 11, 0, 0, 0, 4, 0];
        refused.extend_from_slice(b"No protocol\0\0\0\0\0");
        assert!(
            matches!(parse_setup(&refused), Err(WindowError::NoDisplay(reason)) if reason.contains("No protocol"))
        );
    }

    #[test]
    fn requests_are_encoded_byte_for_byte() {
        assert_eq!(
            map_window(0x0040_0001).unwrap(),
            vec![8, 0, 2, 0, 1, 0, 0x40, 0]
        );
        let mut intern = vec![16, 0, 5, 0, 12, 0, 0, 0];
        intern.extend_from_slice(b"WM_PROTOCOLS");
        assert_eq!(intern_atom("WM_PROTOCOLS").unwrap(), intern);
        let create = create_window(0x0040_0001, 0x123, 1280, 800, 0, EVENT_KEY_PRESS).unwrap();
        assert_eq!(&create[..4], &[1, 0, 10, 0]);
        assert_eq!(&create[16..20], &[0, 5, 0x20, 3]);
        assert_eq!(&create[28..32], &[0x02, 0x08, 0, 0]);
        assert_eq!(create.len(), 40);
        let property =
            change_property(1, ATOM_WM_NAME, ATOM_STRING, PropertyData::Bytes(b"EggDB")).unwrap();
        assert_eq!(&property[..4], &[18, 0, 8, 0]);
        assert_eq!(property[16], 8);
        assert_eq!(&property[20..24], &[5, 0, 0, 0]);
        assert_eq!(&property[24..32], b"EggDB\0\0\0");
        let words = change_property(1, 2, ATOM_ATOM, PropertyData::Words(&[7])).unwrap();
        assert_eq!(&words[..4], &[18, 0, 7, 0]);
        assert_eq!(&words[24..28], &[7, 0, 0, 0]);
        assert_eq!(
            create_gc(2, 1).unwrap(),
            vec![55, 0, 4, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            get_keyboard_mapping(8, 248).unwrap(),
            vec![101, 0, 2, 0, 8, 248, 0, 0]
        );
        let target = ImageTarget {
            drawable: 1,
            gc: 2,
            depth: 24,
        };
        let image = put_image(target, 2, 1, 0, 3, &[9; 8]).unwrap();
        assert_eq!(&image[..4], &[72, 2, 8, 0]);
        assert_eq!(&image[12..24], &[2, 0, 1, 0, 0, 0, 3, 0, 0, 24, 0, 0]);
        assert_eq!(image.len(), 32);
        assert!(put_image(target, 1, 1, 0, 0, &vec![0; 300_000]).is_err());
        assert_eq!(
            translate_coordinates(0x0040_0001, 0x123).unwrap(),
            vec![40, 0, 4, 0, 1, 0, 0x40, 0, 0x23, 1, 0, 0, 0, 0, 0, 0]
        );
        let mut translated = vec![0u8; 32];
        translated[0] = 1;
        translated[12..14].copy_from_slice(&(-20i16).to_le_bytes());
        translated[14..16].copy_from_slice(&300i16.to_le_bytes());
        assert_eq!(
            parse_translate_coordinates_reply(&translated).unwrap(),
            (-20, 300)
        );
        assert!(parse_translate_coordinates_reply(&translated[..13]).is_err());
        let property_request = get_property(0x123, ATOM_RESOURCE_MANAGER, 100).unwrap();
        assert_eq!(&property_request[..4], &[20, 0, 6, 0]);
        assert_eq!(
            minimum_size_hints(800, 560)[..7],
            [16, 0, 0, 0, 0, 800, 560]
        );
    }

    #[test]
    fn put_image_is_chunked_under_the_maximum_request_length() {
        assert_eq!(rows_per_put_image(1280, 0xFFFF), Some(51));
        assert_eq!(rows_per_put_image(1, 0xFFFF), Some(65529));
        assert_eq!(rows_per_put_image(100_000, 0xFFFF), None);
        assert_eq!(rows_per_put_image(0, 0xFFFF), None);
        assert_eq!(rows_per_put_image(10, 0), None);
    }

    fn packet(code: u8, fill: impl Fn(&mut [u8; 32])) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = code;
        fill(&mut bytes);
        bytes
    }

    #[test]
    fn events_replies_and_errors_are_decoded() {
        let key = packet(2, |bytes| {
            bytes[1] = 23;
            bytes[28] = 0x05;
        });
        assert_eq!(
            parse_packet(&key),
            Packet::KeyPress {
                keycode: 23,
                state: 5
            }
        );
        let modifiers = modifiers_from_state(5);
        assert!(modifiers.shift && modifiers.control && !modifiers.alt);
        let configure = packet(22, |bytes| {
            bytes[8..12].copy_from_slice(&7u32.to_le_bytes());
            bytes[20..22].copy_from_slice(&1024u16.to_le_bytes());
            bytes[22..24].copy_from_slice(&700u16.to_le_bytes());
        });
        assert_eq!(
            parse_packet(&configure),
            Packet::ConfigureNotify {
                window: 7,
                width: 1024,
                height: 700
            }
        );
        let close = packet(33 | 0x80, |bytes| {
            bytes[1] = 32;
            bytes[4..8].copy_from_slice(&7u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&300u32.to_le_bytes());
            bytes[12..16].copy_from_slice(&301u32.to_le_bytes());
        });
        assert_eq!(
            parse_packet(&close),
            Packet::ClientMessage {
                window: 7,
                kind: 300,
                first: 301
            }
        );
        let expose = packet(12, |bytes| bytes[16] = 2);
        assert_eq!(
            parse_packet(&expose),
            Packet::Expose {
                window: 0,
                count: 2
            }
        );
        let error = packet(0, |bytes| {
            bytes[1] = 16;
            bytes[2] = 9;
            bytes[10] = 72;
        });
        assert_eq!(
            parse_packet(&error),
            Packet::Error {
                code: 16,
                sequence: 9,
                major_opcode: 72
            }
        );
        let mapping = packet(34, |bytes| bytes[4] = 1);
        assert_eq!(parse_packet(&mapping), Packet::KeyboardMappingChanged);
        let reply = packet(1, |bytes| {
            bytes[2] = 4;
            bytes[4] = 3;
        });
        assert_eq!(parse_packet(&reply), Packet::Reply { sequence: 4 });
        assert_eq!(packet_remaining(&reply), 12);
        assert_eq!(packet_sequence(&reply), 4);
        assert_eq!(packet_remaining(&key), 0);
    }

    #[test]
    fn replies_are_decoded_and_short_ones_are_errors() {
        let mut atom = [0u8; 32];
        atom[0] = 1;
        atom[8..12].copy_from_slice(&301u32.to_le_bytes());
        assert_eq!(parse_intern_atom_reply(&atom), Ok(301));
        assert!(parse_intern_atom_reply(&atom[..10]).is_err());
        let mut mapping = vec![0u8; 32];
        mapping[0] = 1;
        mapping[1] = 2;
        mapping[4] = 4;
        for keysym in [0xFF09u32, 0xFE20, u32::from(b'q'), u32::from(b'Q')] {
            mapping.extend_from_slice(&keysym.to_le_bytes());
        }
        let parsed = parse_keyboard_mapping_reply(&mapping, 23).unwrap();
        assert_eq!(parsed.keysym(23, false), Some(0xFF09));
        assert_eq!(parsed.keysym(23, true), Some(0xFE20));
        assert_eq!(parsed.keysym(24, true), Some(u32::from(b'Q')));
        assert_eq!(parsed.keysym(25, false), None);
        assert_eq!(parsed.keysym(3, false), None);
        assert!(parse_keyboard_mapping_reply(&mapping[..40], 23).is_err());
        let mut property = vec![0u8; 32];
        property[0] = 1;
        property[1] = 8;
        property[16] = 12;
        property.extend_from_slice(b"Xft.dpi:\t192");
        assert_eq!(parse_property_reply(&property), Ok(&b"Xft.dpi:\t192"[..]));
        assert!(parse_property_reply(&property[..40]).is_err());
        let value = parse_property_reply(&property).unwrap();
        assert_eq!(scale_from_resources(value), Some(2.0));
        assert_eq!(
            scale_from_resources(b"Xft.antialias: 1\nXft.dpi: 120\n"),
            Some(1.25)
        );
        assert_eq!(scale_from_resources(b"Xft.dpi: lots"), None);
        assert_eq!(scale_from_resources(b"Xft.dpi: 100000"), None);
    }

    #[test]
    fn key_symbols_map_to_keys() {
        assert_eq!(key_from_keysym(0xFF09), Some(Key::Tab));
        assert_eq!(key_from_keysym(0xFE20), Some(Key::Tab));
        assert_eq!(key_from_keysym(0xFF0D), Some(Key::Enter));
        assert_eq!(key_from_keysym(0xFF1B), Some(Key::Escape));
        assert_eq!(key_from_keysym(u32::from(b'q')), Some(Key::Character('q')));
        assert_eq!(key_from_keysym(0x0100_00E9), Some(Key::Character('é')));
        assert_eq!(key_from_keysym(0xFFE1), None);
        assert_eq!(key_from_keysym(0x0100_D800), None);
    }

    #[test]
    fn hostile_packets_decode_without_panicking() {
        let mut state = 0x1234_5678u32;
        for _ in 0..2000 {
            let mut bytes = [0u8; 32];
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *byte = state as u8;
            }
            let _ = parse_packet(&bytes);
            let _ = packet_remaining(&bytes);
            let _ = parse_setup(&bytes);
            let _ = parse_property_reply(&bytes);
            let _ = parse_keyboard_mapping_reply(&bytes, 8);
        }
    }

    #[test]
    fn selection_requests_are_encoded_byte_for_byte() {
        assert_eq!(
            set_selection_owner(0x0040_0001, 0x1A2, CURRENT_TIME).unwrap(),
            vec![22, 0, 4, 0, 1, 0, 0x40, 0, 0xA2, 1, 0, 0, 0, 0, 0, 0]
        );
        let convert = convert_selection(7, 8, 9, 10, 0).unwrap();
        assert_eq!(&convert[..4], &[24, 0, 6, 0]);
        assert_eq!(&convert[4..8], &7u32.to_le_bytes());
        assert_eq!(&convert[16..20], &10u32.to_le_bytes());
        assert_eq!(convert.len(), 24);
        let notify = selection_notify_event(5, 6, 7, 8, ATOM_NONE);
        assert_eq!(notify[0], EVENT_CODE_SELECTION_NOTIFY);
        assert_eq!(&notify[8..12], &6u32.to_le_bytes());
        assert_eq!(&notify[20..24], &[0, 0, 0, 0]);
        let sent = send_event(6, &notify).unwrap();
        assert_eq!(&sent[..4], &[25, 0, 11, 0]);
        assert_eq!(&sent[12..44], &notify);
        let deleting = get_property_deleting(1, 2, 3).unwrap();
        assert_eq!(&deleting[..2], &[20, 1]);
        assert_eq!(
            delete_property(1, 2).unwrap(),
            vec![19, 0, 3, 0, 1, 0, 0, 0, 2, 0, 0, 0]
        );
    }

    #[test]
    fn pointer_and_selection_events_parse() {
        let mut press = [0u8; 32];
        press[0] = 4;
        press[1] = 1;
        press[24..26].copy_from_slice(&(-3i16).to_le_bytes());
        press[26..28].copy_from_slice(&120i16.to_le_bytes());
        press[28..30].copy_from_slice(&0x0005u16.to_le_bytes());
        assert_eq!(
            parse_packet(&press),
            Packet::ButtonPress {
                detail: 1,
                x: -3,
                y: 120,
                state: 5
            }
        );
        press[0] = 5;
        assert!(matches!(
            parse_packet(&press),
            Packet::ButtonRelease { detail: 1, .. }
        ));
        press[0] = 6;
        assert_eq!(parse_packet(&press), Packet::MotionNotify { x: -3, y: 120 });
        let mut request = [0u8; 32];
        request[0] = 30;
        for (at, value) in [(4, 1u32), (8, 2), (12, 3), (16, 4), (20, 5), (24, 6)] {
            request[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            parse_packet(&request),
            Packet::SelectionRequest {
                time: 1,
                owner: 2,
                requestor: 3,
                selection: 4,
                target: 5,
                property: 6,
            }
        );
        let notify = selection_notify_event(0, 9, 10, 11, 12);
        assert_eq!(
            parse_packet(&notify),
            Packet::SelectionNotify {
                requestor: 9,
                selection: 10,
                target: 11,
                property: 12
            }
        );
        let mut leave = [0u8; 32];
        leave[0] = 8;
        leave[12..16].copy_from_slice(&77u32.to_le_bytes());
        assert_eq!(
            parse_packet(&leave),
            Packet::LeaveNotify {
                window: 77,
                mode: 0
            }
        );
    }

    #[test]
    fn button_codes_are_buttons_and_wheel_steps() {
        assert_eq!(button_code(1), ButtonCode::Button(PointerButton::Primary));
        assert_eq!(button_code(3), ButtonCode::Button(PointerButton::Secondary));
        assert_eq!(button_code(2), ButtonCode::Button(PointerButton::Middle));
        assert_eq!(
            button_code(5),
            ButtonCode::Wheel {
                lines_down: 1.0,
                lines_right: 0.0
            }
        );
        assert_eq!(
            button_code(6),
            ButtonCode::Wheel {
                lines_down: 0.0,
                lines_right: -1.0
            }
        );
        assert_eq!(button_code(9), ButtonCode::Other);
    }
}
