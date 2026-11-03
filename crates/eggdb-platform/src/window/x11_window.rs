use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::atspi_bridge::{self, AtspiBridge};
use super::x11::{self, ImageTarget, KeyboardMapping, Packet, PixelLayout, PropertyData};
use super::{AccessNode, KeyPress, Window, WindowError, WindowEvent, WindowOptions, check_frame};

const LARGEST_REPLY: usize = 16 << 20;
const RESOURCES_UNITS: u32 = 1 << 16;
const SHORTEST_WAIT: Duration = Duration::from_millis(1);
const BACKLOG_RETRY: Duration = Duration::from_millis(10);

fn io_error(error: &std::io::Error) -> WindowError {
    WindowError::Io(error.to_string())
}

fn hostname() -> Option<Vec<u8>> {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|path| std::fs::read(path).ok())
        .map(|bytes| bytes.trim_ascii().to_vec())
        .filter(|name| !name.is_empty())
}

fn xauthority_path() -> Option<PathBuf> {
    std::env::var_os("XAUTHORITY")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority")))
}

fn cookie_for(display: u32) -> Option<x11::Cookie> {
    let bytes = std::fs::read(xauthority_path()?).ok()?;
    x11::find_cookie(&bytes, display, hostname().as_deref())
}

#[derive(Debug)]
pub(crate) struct X11Window {
    stream: UnixStream,
    incoming: Vec<u8>,
    pending: VecDeque<WindowEvent>,
    sequence: u16,
    window: u32,
    root: u32,
    target: ImageTarget,
    layout: PixelLayout,
    maximum_request_length: u16,
    min_keycode: u8,
    max_keycode: u8,
    keyboard: KeyboardMapping,
    keyboard_stale: bool,
    wm_protocols: u32,
    wm_delete_window: u32,
    size: (u32, u32),
    scale: f32,
    accessibility: Option<AtspiBridge>,
    origin_stale: bool,
}

impl X11Window {
    pub(crate) fn open(options: &WindowOptions) -> Result<Self, WindowError> {
        let display = std::env::var("DISPLAY")
            .map_err(|_| WindowError::NoDisplay("DISPLAY is not set".to_owned()))?;
        let name = x11::parse_display(&display)?;
        let path = name.socket_path();
        let mut stream = UnixStream::connect(&path)
            .map_err(|error| WindowError::NoDisplay(format!("{path}: {error}")))?;
        let cookie = cookie_for(name.number);
        stream
            .write_all(&x11::encode_setup_request(cookie.as_ref()))
            .map_err(|error| io_error(&error))?;
        let mut header = [0u8; 8];
        stream
            .read_exact(&mut header)
            .map_err(|error| io_error(&error))?;
        let mut reply = header.to_vec();
        reply.resize(8 + x11::setup_reply_remaining(&header), 0);
        stream
            .read_exact(&mut reply[8..])
            .map_err(|error| io_error(&error))?;
        let setup = x11::parse_setup(&reply)?;
        let screen = usize::try_from(name.screen)
            .ok()
            .and_then(|index| setup.screens.get(index))
            .ok_or_else(|| {
                WindowError::NoDisplay(format!("the display has no screen {}", name.screen))
            })?
            .clone();
        let layout = PixelLayout::for_screen(&setup, &screen)?;
        let (Some(window), Some(gc)) = (setup.resource_id(1), setup.resource_id(2)) else {
            return Err(WindowError::Protocol(
                "the server left no resource ids for this client".to_owned(),
            ));
        };
        let mut this = Self {
            stream,
            incoming: Vec::new(),
            pending: VecDeque::new(),
            sequence: 0,
            window,
            root: screen.root,
            target: ImageTarget {
                drawable: window,
                gc,
                depth: screen.root_depth,
            },
            layout,
            maximum_request_length: setup.maximum_request_length,
            min_keycode: setup.min_keycode,
            max_keycode: setup.max_keycode,
            keyboard: KeyboardMapping::default(),
            keyboard_stale: false,
            wm_protocols: 0,
            wm_delete_window: 0,
            size: (0, 0),
            scale: 1.0,
            accessibility: None,
            origin_stale: false,
        };
        this.scale = this.read_scale(screen.root)?;
        this.wm_protocols = this.intern("WM_PROTOCOLS")?;
        this.wm_delete_window = this.intern("WM_DELETE_WINDOW")?;
        let net_wm_name = this.intern("_NET_WM_NAME")?;
        let utf8_string = this.intern("UTF8_STRING")?;
        this.refresh_keyboard()?;
        let scale = this.scale;
        let device = |logical: u32| {
            let scaled = (logical as f32 * scale).round();
            u16::try_from(scaled as u32).unwrap_or(u16::MAX).max(1)
        };
        let (width, height) = (device(options.width), device(options.height));
        let (min_width, min_height) = (device(options.min_width), device(options.min_height));
        this.size = (u32::from(width), u32::from(height));
        let event_mask = x11::EVENT_KEY_PRESS
            | x11::EVENT_EXPOSURE
            | x11::EVENT_STRUCTURE_NOTIFY
            | x11::EVENT_FOCUS_CHANGE;
        this.send(&x11::create_window(
            window,
            screen.root,
            width,
            height,
            screen.black_pixel,
            event_mask,
        )?)?;
        let title = options.title.as_bytes();
        this.send(&x11::change_property(
            window,
            x11::ATOM_WM_NAME,
            x11::ATOM_STRING,
            PropertyData::Bytes(title),
        )?)?;
        this.send(&x11::change_property(
            window,
            net_wm_name,
            utf8_string,
            PropertyData::Bytes(title),
        )?)?;
        this.send(&x11::change_property(
            window,
            this.wm_protocols,
            x11::ATOM_ATOM,
            PropertyData::Words(&[this.wm_delete_window]),
        )?)?;
        this.send(&x11::change_property(
            window,
            x11::ATOM_WM_NORMAL_HINTS,
            x11::ATOM_WM_SIZE_HINTS,
            PropertyData::Words(&x11::minimum_size_hints(
                u32::from(min_width),
                u32::from(min_height),
            )),
        )?)?;
        this.send(&x11::create_gc(gc, window)?)?;
        this.send(&x11::map_window(window)?)?;
        this.accessibility = AtspiBridge::connect(&options.title);
        this.origin_stale = this.accessibility.is_some();
        Ok(this)
    }

    fn update_accessibility(&mut self, update: impl FnOnce(&mut AtspiBridge) -> bool) {
        if let Some(bridge) = &mut self.accessibility
            && !update(bridge)
        {
            self.accessibility = None;
        }
    }

    fn refresh_origin(&mut self) -> Result<(), WindowError> {
        self.origin_stale = false;
        let sequence = self.send(&x11::translate_coordinates(self.window, self.root)?)?;
        let reply = self.reply(sequence)?;
        let origin = x11::parse_translate_coordinates_reply(&reply)?;
        let scale = self.scale;
        if let Some(bridge) = &mut self.accessibility {
            bridge.set_geometry(scale, origin);
        }
        Ok(())
    }

    fn read_packet_serving_accessibility(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<Option<Vec<u8>>, WindowError> {
        loop {
            let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let Some(bridge) = &mut self.accessibility else {
                return self.read_packet(left);
            };
            if !bridge.pump() {
                self.accessibility = None;
                continue;
            }
            let requests = bridge.take_requests();
            let (bus, backlog) = (bridge.fd(), bridge.has_backlog());
            if !requests.is_empty() {
                self.pending
                    .extend(requests.into_iter().map(WindowEvent::AccessRequested));
                return Ok(Some(Vec::new()));
            }
            if let Some(packet) = self.read_packet(Some(Duration::ZERO))? {
                return Ok(Some(packet));
            }
            let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if left == Some(Duration::ZERO) {
                return Ok(None);
            }
            let wait = if backlog {
                Some(left.map_or(BACKLOG_RETRY, |left| left.min(BACKLOG_RETRY)))
            } else {
                left
            };
            atspi_bridge::wait_for_input(self.stream.as_raw_fd(), bus, wait)
                .map_err(|error| io_error(&error))?;
        }
    }

    fn send(&mut self, request: &[u8]) -> Result<u16, WindowError> {
        self.stream
            .write_all(request)
            .map_err(|error| io_error(&error))?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(self.sequence)
    }

    fn fill(&mut self, wanted: usize) -> Result<bool, WindowError> {
        let mut chunk = [0u8; 4096];
        while self.incoming.len() < wanted {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(WindowError::Io(
                        "the X server closed the connection".to_owned(),
                    ));
                }
                Ok(read) => self
                    .incoming
                    .extend_from_slice(chunk.get(..read).unwrap_or(&[])),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(false);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
        Ok(true)
    }

    fn read_packet(&mut self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, WindowError> {
        self.stream
            .set_read_timeout(timeout.map(|wait| wait.max(SHORTEST_WAIT)))
            .map_err(|error| io_error(&error))?;
        if !self.fill(32)? {
            return Ok(None);
        }
        let mut header = [0u8; 32];
        header.copy_from_slice(self.incoming.get(..32).unwrap_or(&[0; 32]));
        let remaining = x11::packet_remaining(&header);
        if remaining > LARGEST_REPLY {
            return Err(WindowError::Protocol(format!(
                "a reply of {remaining} bytes is larger than any this client asks for"
            )));
        }
        if !self.fill(32 + remaining)? {
            return Ok(None);
        }
        let rest = self.incoming.split_off(32 + remaining);
        Ok(Some(std::mem::replace(&mut self.incoming, rest)))
    }

    fn reply(&mut self, sequence: u16) -> Result<Vec<u8>, WindowError> {
        loop {
            let Some(packet) = self.read_packet(None)? else {
                continue;
            };
            let mut header = [0u8; 32];
            header.copy_from_slice(packet.get(..32).unwrap_or(&[0; 32]));
            match x11::parse_packet(&header) {
                Packet::Reply { sequence: got } if got == sequence => return Ok(packet),
                Packet::Reply { .. } => {}
                packet => {
                    if let Some(event) = self.translate(packet)? {
                        self.pending.push_back(event);
                    }
                }
            }
        }
    }

    fn intern(&mut self, name: &str) -> Result<u32, WindowError> {
        let sequence = self.send(&x11::intern_atom(name)?)?;
        let reply = self.reply(sequence)?;
        x11::parse_intern_atom_reply(&reply)
    }

    fn read_scale(&mut self, root: u32) -> Result<f32, WindowError> {
        let sequence = self.send(&x11::get_property(
            root,
            x11::ATOM_RESOURCE_MANAGER,
            RESOURCES_UNITS,
        )?)?;
        let reply = self.reply(sequence)?;
        let resources = x11::parse_property_reply(&reply)?;
        Ok(x11::scale_from_resources(resources).unwrap_or(1.0))
    }

    fn refresh_keyboard(&mut self) -> Result<(), WindowError> {
        let count = self
            .max_keycode
            .saturating_sub(self.min_keycode)
            .saturating_add(1);
        let sequence = self.send(&x11::get_keyboard_mapping(self.min_keycode, count)?)?;
        let reply = self.reply(sequence)?;
        self.keyboard = x11::parse_keyboard_mapping_reply(&reply, self.min_keycode)?;
        self.keyboard_stale = false;
        Ok(())
    }

    fn translate(&mut self, packet: Packet) -> Result<Option<WindowEvent>, WindowError> {
        Ok(match packet {
            Packet::Error {
                code,
                sequence,
                major_opcode,
            } => {
                return Err(WindowError::Protocol(format!(
                    "the X server refused request {sequence} (opcode {major_opcode}) with error {code}"
                )));
            }
            Packet::KeyPress { keycode, state } => {
                let modifiers = x11::modifiers_from_state(state);
                self.keyboard
                    .keysym(keycode, modifiers.shift)
                    .and_then(x11::key_from_keysym)
                    .map(|key| WindowEvent::Key(KeyPress { key, modifiers }))
            }
            Packet::Expose { window, count } if window == self.window && count == 0 => {
                Some(WindowEvent::Redraw)
            }
            Packet::ConfigureNotify {
                window,
                width,
                height,
            } if window == self.window => {
                self.origin_stale = self.accessibility.is_some();
                let size = (u32::from(width), u32::from(height));
                (size != self.size).then(|| {
                    self.size = size;
                    WindowEvent::Resized {
                        width: size.0,
                        height: size.1,
                    }
                })
            }
            Packet::ClientMessage {
                window,
                kind,
                first,
            } if window == self.window
                && kind == self.wm_protocols
                && first == self.wm_delete_window =>
            {
                Some(WindowEvent::CloseRequested)
            }
            Packet::FocusIn => {
                self.update_accessibility(|bridge| bridge.set_window_active(true));
                Some(WindowEvent::Focused(true))
            }
            Packet::FocusOut => {
                self.update_accessibility(|bridge| bridge.set_window_active(false));
                Some(WindowEvent::Focused(false))
            }
            Packet::KeyboardMappingChanged => {
                self.keyboard_stale = true;
                None
            }
            Packet::Reply { .. }
            | Packet::Expose { .. }
            | Packet::ConfigureNotify { .. }
            | Packet::ClientMessage { .. }
            | Packet::Other(_) => None,
        })
    }
}

impl Window for X11Window {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn scale(&self) -> f32 {
        self.scale
    }

    fn present(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), WindowError> {
        check_frame(width, height, rgba)?;
        let too_large = || {
            WindowError::InvalidInput(format!(
                "a {width} × {height} frame is larger than an X11 window can take"
            ))
        };
        let (Ok(image_width), Ok(_)) = (u16::try_from(width), i16::try_from(height)) else {
            return Err(too_large());
        };
        let rows =
            x11::rows_per_put_image(width, self.maximum_request_length).ok_or_else(too_large)?;
        let row_bytes = width as usize * 4;
        let mut top = 0u32;
        while top < height {
            let count = rows.min(height - top);
            let start = top as usize * row_bytes;
            let end = start + count as usize * row_bytes;
            let pixels = self.layout.encode(rgba.get(start..end).unwrap_or(&[]));
            let (Ok(chunk_height), Ok(y)) = (u16::try_from(count), i16::try_from(top)) else {
                return Err(too_large());
            };
            self.send(&x11::put_image(
                self.target,
                image_width,
                chunk_height,
                0,
                y,
                &pixels,
            )?)?;
            top += count;
        }
        self.stream.flush().map_err(|error| io_error(&error))
    }

    fn next_event(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<WindowEvent>, WindowError> {
        let deadline = timeout.and_then(|wait| Instant::now().checked_add(wait));
        loop {
            if self.keyboard_stale {
                self.refresh_keyboard()?;
            }
            if self.origin_stale {
                self.refresh_origin()?;
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let packet = if self.accessibility.is_some() {
                self.read_packet_serving_accessibility(deadline)?
            } else {
                self.read_packet(timeout)?
            };
            let Some(packet) = packet else {
                return Ok(None);
            };
            if packet.is_empty() {
                continue;
            }
            let mut header = [0u8; 32];
            header.copy_from_slice(packet.get(..32).unwrap_or(&[0; 32]));
            if let Some(event) = self.translate(x11::parse_packet(&header))? {
                return Ok(Some(event));
            }
        }
    }

    fn set_accessibility_tree(&mut self, nodes: &[AccessNode]) {
        self.update_accessibility(|bridge| bridge.set_tree(nodes));
    }
}
