use std::ffi::{c_int, c_short};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::atspi::{self, AtspiApplication};
use super::{AccessNode, AccessRequest};
use crate::dbus::{self, BusAddress, Message, MessageKind};

const SETUP_WAIT: Duration = Duration::from_millis(500);
const LONGEST_AUTHENTICATION_LINE: usize = 1024;
const LARGEST_BACKLOG: usize = 8 << 20;
const POLLIN: c_short = 0x1;

#[cfg(target_os = "linux")]
type PollCount = std::ffi::c_ulong;
#[cfg(not(target_os = "linux"))]
type PollCount = std::ffi::c_uint;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

unsafe extern "C" {
    fn getuid() -> u32;
    fn poll(fds: *mut PollFd, count: PollCount, timeout_ms: c_int) -> c_int;
}

fn connect(address: &BusAddress) -> Option<UnixStream> {
    match address {
        BusAddress::UnixPath(path) => UnixStream::connect(path).ok(),
        BusAddress::UnixAbstract(name) => connect_abstract(name),
    }
}

#[cfg(target_os = "linux")]
fn connect_abstract(name: &str) -> Option<UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).ok()?;
    UnixStream::connect_addr(&address).ok()
}

#[cfg(not(target_os = "linux"))]
fn connect_abstract(_name: &str) -> Option<UnixStream> {
    None
}

fn broken(what: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, what.to_owned())
}

#[derive(Debug)]
struct BusConnection {
    stream: UnixStream,
    incoming: Vec<u8>,
    outgoing: Vec<u8>,
    serial: u32,
}

impl BusConnection {
    fn open(address: &BusAddress) -> Option<Self> {
        let stream = connect(address)?;
        stream.set_read_timeout(Some(SETUP_WAIT)).ok()?;
        stream.set_write_timeout(Some(SETUP_WAIT)).ok()?;
        let mut bus = Self {
            stream,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            serial: 0,
        };
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { getuid() };
        bus.stream.write_all(&dbus::sasl_external(uid)).ok()?;
        let line = bus.read_line()?;
        dbus::sasl_accepted(&line).ok()?;
        bus.stream.write_all(dbus::SASL_BEGIN).ok()?;
        Some(bus)
    }

    fn read_line(&mut self) -> Option<String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            if line.len() >= LONGEST_AUTHENTICATION_LINE {
                return None;
            }
            match self.stream.read(&mut byte) {
                Ok(1) => line.extend_from_slice(&byte),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                _ => return None,
            }
        }
        String::from_utf8(line).ok()
    }

    fn queue(&mut self, mut message: Message) -> Option<u32> {
        self.serial = self.serial.wrapping_add(1).max(1);
        message.serial = self.serial;
        let bytes = message.encode().ok()?;
        self.outgoing.extend_from_slice(&bytes);
        Some(self.serial)
    }

    fn flush(&mut self) -> io::Result<()> {
        while !self.outgoing.is_empty() {
            match self.stream.write(&self.outgoing) {
                Ok(0) => return Err(broken("the accessibility bus stopped reading")),
                Ok(written) => {
                    self.outgoing.drain(..written.min(self.outgoing.len()));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if self.outgoing.len() > LARGEST_BACKLOG {
            return Err(broken("the accessibility bus fell too far behind"));
        }
        Ok(())
    }

    fn take_message(&mut self) -> io::Result<Option<Message>> {
        let length =
            dbus::message_length(&self.incoming).map_err(|error| broken(&error.to_string()))?;
        let Some(length) = length.filter(|&length| self.incoming.len() >= length) else {
            return Ok(None);
        };
        let rest = self.incoming.split_off(length);
        let bytes = std::mem::replace(&mut self.incoming, rest);
        Message::decode(&bytes)
            .map(Some)
            .map_err(|error| broken(&error.to_string()))
    }

    fn receive(&mut self) -> io::Result<Option<Message>> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(message) = self.take_message()? {
                return Ok(Some(message));
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(broken("the bus closed the connection")),
                Ok(read) => self
                    .incoming
                    .extend_from_slice(chunk.get(..read).unwrap_or_default()),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn call(&mut self, message: Message) -> Option<Message> {
        let serial = self.queue(message)?;
        self.flush().ok()?;
        if !self.outgoing.is_empty() {
            return None;
        }
        let deadline = Instant::now().checked_add(SETUP_WAIT.saturating_mul(2))?;
        while Instant::now() < deadline {
            match self.receive() {
                Ok(Some(reply)) if reply.reply_serial == Some(serial) => {
                    return (reply.kind == MessageKind::MethodReturn).then_some(reply);
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn hello(&mut self) -> Option<String> {
        let reply = self.call(Message::hello(0))?;
        reply.body.first()?.as_str().map(str::to_owned)
    }
}

fn accessibility_bus_address(session: &BusAddress) -> Option<String> {
    let mut bus = BusConnection::open(session)?;
    bus.hello()?;
    let reply = bus.call(Message::method_call(
        0,
        atspi::ACCESSIBILITY_BUS_NAME,
        atspi::ACCESSIBILITY_BUS_PATH,
        atspi::ACCESSIBILITY_BUS_NAME,
        "GetAddress",
        Vec::new(),
    ))?;
    reply.body.first()?.as_str().map(str::to_owned)
}

#[derive(Debug)]
pub(crate) struct AtspiBridge {
    bus: BusConnection,
    application: AtspiApplication,
    embed_serial: Option<u32>,
}

impl AtspiBridge {
    pub(crate) fn connect(application_name: &str) -> Option<Self> {
        if std::env::var_os("NO_AT_BRIDGE").is_some_and(|value| value == "1") {
            return None;
        }
        let session = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
        let address = dbus::session_bus_addresses(session.as_deref(), runtime_dir.as_deref())
            .iter()
            .find_map(accessibility_bus_address)?;
        let mut bus = dbus::parse_addresses(&address)
            .iter()
            .find_map(BusConnection::open)?;
        let unique_name = bus.hello()?;
        bus.stream.set_nonblocking(true).ok()?;
        let application = AtspiApplication::new(&unique_name, application_name);
        let embed_serial = bus.queue(application.embed_call());
        let mut bridge = Self {
            bus,
            application,
            embed_serial,
        };
        bridge.bus.flush().ok()?;
        Some(bridge)
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.bus.stream.as_raw_fd()
    }

    pub(crate) fn has_backlog(&self) -> bool {
        !self.bus.outgoing.is_empty()
    }

    pub(crate) fn pump(&mut self) -> bool {
        loop {
            match self.bus.receive() {
                Ok(Some(message)) => self.dispatch(&message),
                Ok(None) => break,
                Err(_) => return false,
            }
        }
        self.bus.flush().is_ok()
    }

    pub(crate) fn set_tree(&mut self, nodes: &[AccessNode]) -> bool {
        let events = self.application.set_tree(nodes);
        self.send(events)
    }

    pub(crate) fn set_window_active(&mut self, active: bool) -> bool {
        let events = self.application.set_window_active(active);
        self.send(events)
    }

    pub(crate) fn set_geometry(&mut self, scale: f32, origin: (i32, i32)) {
        self.application.set_geometry(scale, origin);
    }

    pub(crate) fn take_requests(&mut self) -> Vec<AccessRequest> {
        self.application.take_requests()
    }

    fn send(&mut self, messages: Vec<Message>) -> bool {
        for message in messages {
            self.bus.queue(message);
        }
        self.bus.flush().is_ok()
    }

    fn dispatch(&mut self, message: &Message) {
        match message.kind {
            MessageKind::MethodCall => {
                if let Some(reply) = self.application.answer(message) {
                    self.bus.queue(reply);
                }
            }
            MessageKind::MethodReturn | MessageKind::Error
                if self.embed_serial.is_some() && message.reply_serial == self.embed_serial =>
            {
                if message.kind == MessageKind::MethodReturn {
                    self.application.embedded(message);
                }
                self.embed_serial = None;
            }
            _ => {}
        }
    }
}

pub(crate) fn wait_for_input(
    first: RawFd,
    second: RawFd,
    timeout: Option<Duration>,
) -> io::Result<()> {
    let mut fds = [
        PollFd {
            fd: first,
            events: POLLIN,
            revents: 0,
        },
        PollFd {
            fd: second,
            events: POLLIN,
            revents: 0,
        },
    ];
    let timeout_ms = timeout.map_or(-1, |wait| {
        c_int::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(c_int::MAX)
    });
    // SAFETY: `fds` holds two initialised pollfds the kernel may update, and the count is two.
    let ready = unsafe { poll(fds.as_mut_ptr(), 2, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::Value;
    use crate::window::{AccessRect, AccessRole, AccessState};

    fn pair() -> (BusConnection, UnixStream) {
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        ours.set_nonblocking(true).expect("non-blocking");
        let bus = BusConnection {
            stream: ours,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            serial: 0,
        };
        (bus, theirs)
    }

    fn read_message(peer: &mut UnixStream) -> Message {
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut bytes = vec![0u8; 16];
        peer.read_exact(&mut bytes).unwrap();
        let length = dbus::message_length(&bytes).unwrap().unwrap();
        bytes.resize(length, 0);
        peer.read_exact(&mut bytes[16..]).unwrap();
        Message::decode(&bytes).unwrap()
    }

    #[test]
    fn the_bridge_answers_calls_and_records_the_desktop_over_a_socket() {
        let (bus, mut peer) = pair();
        let mut bridge = AtspiBridge {
            bus,
            application: AtspiApplication::new(":1.5", "eggdb-gui"),
            embed_serial: None,
        };
        bridge.embed_serial = bridge.bus.queue(bridge.application.embed_call());
        assert!(bridge.pump());
        let embed = read_message(&mut peer);
        assert_eq!(embed.member.as_deref(), Some("Embed"));
        let mut desktop =
            Message::method_return(1, &embed, vec![atspi::reference(":1.0", atspi::ROOT_PATH)]);
        desktop.sender = Some(":1.0".to_owned());
        let mut call = Message::method_call(
            2,
            ":1.5",
            atspi::ROOT_PATH,
            atspi::ACCESSIBLE_INTERFACE,
            "GetRole",
            Vec::new(),
        );
        call.sender = Some(":1.9".to_owned());
        let mut bytes = desktop.encode().unwrap();
        let call_bytes = call.encode().unwrap();
        bytes.extend_from_slice(call_bytes.get(..10).unwrap());
        peer.write_all(&bytes).unwrap();
        assert!(bridge.pump());
        assert_eq!(bridge.embed_serial, None);
        peer.write_all(call_bytes.get(10..).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while bridge.bus.serial < 2 && Instant::now() < deadline {
            wait_for_input(bridge.fd(), bridge.fd(), Some(Duration::from_millis(50))).unwrap();
            assert!(bridge.pump());
        }
        let reply = read_message(&mut peer);
        assert_eq!(reply.kind, MessageKind::MethodReturn);
        assert_eq!(reply.reply_serial, Some(2));
        assert_eq!(reply.destination.as_deref(), Some(":1.9"));
        assert_eq!(reply.body, vec![Value::UInt32(atspi::ROLE_APPLICATION)]);
        let window = AccessNode {
            id: 1,
            parent: None,
            role: AccessRole::Window,
            name: "Connect".to_owned(),
            value: None,
            placeholder: None,
            state: AccessState {
                focused: true,
                ..AccessState::default()
            },
            bounds: AccessRect::default(),
        };
        assert!(bridge.set_tree(&[window]));
        let added = read_message(&mut peer);
        assert_eq!(added.member.as_deref(), Some("ChildrenChanged"));
        assert_eq!(added.kind, MessageKind::Signal);
    }

    #[test]
    fn a_closed_or_garbled_bus_ends_the_bridge() {
        let (bus, peer) = pair();
        let mut bridge = AtspiBridge {
            bus,
            application: AtspiApplication::new(":1.5", "eggdb-gui"),
            embed_serial: None,
        };
        drop(peer);
        assert!(!bridge.pump());
        let (bus, mut peer) = pair();
        bridge.bus = bus;
        peer.write_all(&[b'X'; 32]).unwrap();
        assert!(!bridge.pump());
    }

    #[test]
    fn authentication_reads_one_line_and_refuses_endless_ones() {
        let (mut bus, mut peer) = pair();
        bus.stream.set_nonblocking(false).unwrap();
        bus.stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        peer.write_all(b"OK 1234abcd\r\nBEGIN").unwrap();
        assert_eq!(bus.read_line().as_deref(), Some("OK 1234abcd\r\n"));
        peer.write_all(&[b'a'; LONGEST_AUTHENTICATION_LINE + 1])
            .unwrap();
        assert_eq!(bus.read_line(), None);
    }
}
