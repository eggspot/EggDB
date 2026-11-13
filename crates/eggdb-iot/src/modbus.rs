//! Modbus TCP polling (D1072): each device's coils, discrete inputs, holding registers and
//! input registers read at a fixed interval and stored as rows of `modbus_values`, one per bit
//! or register, written from the Modbus Application Protocol and Modbus TCP specifications.

use crate::sql::{literal, pause, run, session};
use eggdb_sql::{KeyValueStore, Session};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const MOST_REGISTERS: u16 = 125;
const MOST_BITS: u16 = 2_000;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);

/// One of Modbus's four data tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    /// Read with function 1.
    Coils,
    /// Read with function 2.
    DiscreteInputs,
    /// Read with function 3.
    HoldingRegisters,
    /// Read with function 4.
    InputRegisters,
}

impl Table {
    fn function(self) -> u8 {
        match self {
            Table::Coils => 1,
            Table::DiscreteInputs => 2,
            Table::HoldingRegisters => 3,
            Table::InputRegisters => 4,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Table::Coils => "coil",
            Table::DiscreteInputs => "discrete_input",
            Table::HoldingRegisters => "holding_register",
            Table::InputRegisters => "input_register",
        }
    }

    fn bits(self) -> bool {
        matches!(self, Table::Coils | Table::DiscreteInputs)
    }
}

/// A device and what to read from it, written
/// `tcp://host[:port]|unit=1|holding=0+10|input=100+4|coils=0+16|discrete=0+8|every=1000`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// `host:port`, port 502 when not given.
    pub address: String,
    /// The unit identifier.
    pub unit: u8,
    /// Each range as table, first address and count.
    pub ranges: Vec<(Table, u16, u16)>,
    /// How often to poll.
    pub every: Duration,
}

impl Device {
    /// Reads a device description.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parts = text.split('|');
        let host = parts.next().unwrap_or_default().strip_prefix("tcp://").ok_or_else(|| format!("{text} must start with tcp://host"))?;
        if host.is_empty() {
            return Err(format!("{text} names no host"));
        }
        let address = if host.contains(':') { host.to_owned() } else { format!("{host}:502") };
        let mut device = Self { address, unit: 1, ranges: Vec::new(), every: Duration::from_secs(1) };
        for part in parts {
            let (key, value) = part.split_once('=').ok_or_else(|| format!("{text}: {part} is not key=value"))?;
            let range = |table: Table| -> Result<(Table, u16, u16), String> {
                let (first, count) = value.split_once('+').ok_or_else(|| format!("{text}: write {key}=<first>+<count>"))?;
                let first: u16 = first.parse().map_err(|_| format!("{text}: {first} is not an address"))?;
                let count: u16 = count.parse().map_err(|_| format!("{text}: {count} is not a count"))?;
                let most = if table.bits() { MOST_BITS } else { MOST_REGISTERS };
                if count == 0 || count > most || u32::from(first) + u32::from(count) > 65_536 {
                    return Err(format!("{text}: {key} reads 1 to {most} items within the 65,536 addresses"));
                }
                Ok((table, first, count))
            };
            match key {
                "unit" => device.unit = value.parse().map_err(|_| format!("{text}: the unit is 0 to 255"))?,
                "every" => {
                    let millis: u64 = value.parse().map_err(|_| format!("{text}: every is milliseconds"))?;
                    device.every = Duration::from_millis(millis.max(10));
                }
                "coils" => device.ranges.push(range(Table::Coils)?),
                "discrete" => device.ranges.push(range(Table::DiscreteInputs)?),
                "holding" => device.ranges.push(range(Table::HoldingRegisters)?),
                "input" => device.ranges.push(range(Table::InputRegisters)?),
                other => return Err(format!("{text}: {other} is not unit, every, coils, discrete, holding or input")),
            }
        }
        if device.ranges.is_empty() {
            return Err(format!("{text}: name at least one range, such as |holding=0+10"));
        }
        Ok(device)
    }
}

/// The request for `count` items from `first` of `table`, as transaction `transaction`.
pub fn request(transaction: u16, unit: u8, table: Table, first: u16, count: u16) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[..2].copy_from_slice(&transaction.to_be_bytes());
    out[4..6].copy_from_slice(&6u16.to_be_bytes());
    out[6] = unit;
    out[7] = table.function();
    out[8..10].copy_from_slice(&first.to_be_bytes());
    out[10..12].copy_from_slice(&count.to_be_bytes());
    out
}

/// The values of a response's PDU (after the MBAP header) for `count` items of `table`.
pub fn values(pdu: &[u8], table: Table, count: u16) -> Result<Vec<u16>, String> {
    let (&function, rest) = pdu.split_first().ok_or("an empty response")?;
    if function == table.function() | 0x80 {
        return Err(format!("the device answered exception {}", rest.first().copied().unwrap_or(0)));
    }
    if function != table.function() {
        return Err(format!("the device answered function {function}, not {}", table.function()));
    }
    let (&length, data) = rest.split_first().ok_or("a response without its byte count")?;
    let data = data.get(..usize::from(length)).ok_or("a response shorter than its byte count")?;
    if table.bits() {
        if data.len() * 8 < usize::from(count) {
            return Err("a response with fewer bits than asked".to_owned());
        }
        Ok((0..usize::from(count)).map(|at| u16::from(data[at / 8] >> (at % 8) & 1)).collect())
    } else {
        if data.len() < usize::from(count) * 2 {
            return Err("a response with fewer registers than asked".to_owned());
        }
        Ok(data.chunks_exact(2).take(usize::from(count)).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect())
    }
}

struct Connection {
    stream: TcpStream,
    transaction: u16,
}

impl Connection {
    fn read(&mut self, unit: u8, table: Table, first: u16, count: u16) -> Result<Vec<u16>, String> {
        self.transaction = self.transaction.wrapping_add(1);
        self.stream.write_all(&request(self.transaction, unit, table, first, count)).map_err(|error| error.to_string())?;
        loop {
            let mut header = [0u8; 7];
            self.stream.read_exact(&mut header).map_err(|error| error.to_string())?;
            let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
            if !(2..=260).contains(&length) {
                return Err(format!("a response of {length} bytes"));
            }
            let mut pdu = vec![0u8; length - 1];
            self.stream.read_exact(&mut pdu).map_err(|error| error.to_string())?;
            if u16::from_be_bytes([header[0], header[1]]) == self.transaction {
                return values(&pdu, table, count);
            }
        }
    }
}

/// Polls each device and stores what it reads.
pub struct ModbusPoller<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    database: String,
    devices: Vec<Device>,
}

impl<S: KeyValueStore> std::fmt::Debug for ModbusPoller<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModbusPoller").field("devices", &self.devices).finish_non_exhaustive()
    }
}

impl<'s, S: KeyValueStore> ModbusPoller<'s, S> {
    /// A poller writing into `database` as `user`.
    pub fn new(store: &'s S, user: &str, database: &str, devices: Vec<Device>) -> Self {
        Self { store, user: user.to_owned(), database: database.to_owned(), devices }
    }

    fn session(&self) -> Session<'s> {
        session(self.store, &self.user, &self.database, "modbus")
    }

    /// Creates `modbus_values` when it is missing.
    pub fn prepare(&self) -> Result<(), String> {
        run(&mut self.session(), "CREATE TABLE IF NOT EXISTS modbus_values (received_at timestamptz NOT NULL, device text NOT NULL, unit smallint NOT NULL, kind text NOT NULL, address integer NOT NULL, value integer NOT NULL)")
    }

    fn poll_once(&self, device: &Device, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync)) -> Result<(), String> {
        let stream = TcpStream::connect(&device.address).map_err(|error| format!("cannot reach {}: {error}", device.address))?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|error| error.to_string())?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        let mut connection = Connection { stream, transaction: 0 };
        let mut session = self.session();
        while !stop() {
            let started = Instant::now();
            let mut rows = Vec::new();
            for (table, first, count) in &device.ranges {
                let read = connection.read(device.unit, *table, *first, *count)?;
                for (offset, value) in read.into_iter().enumerate() {
                    rows.push(format!(
                        "(now(), {}, {}, {}, {}, {value})",
                        literal(&device.address),
                        device.unit,
                        literal(table.name()),
                        u32::from(*first) + offset as u32
                    ));
                }
            }
            if let Err(error) = run(&mut session, &format!("INSERT INTO modbus_values (received_at, device, unit, kind, address, value) VALUES {}", rows.join(", "))) {
                log(&format!("{}: {} values were not stored: {error}", device.address, rows.len()));
            }
            pause(device.every.saturating_sub(started.elapsed()), stop);
        }
        Ok(())
    }

    fn follow(&self, device: &Device, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync)) {
        let mut wait = FIRST_PAUSE;
        while !stop() {
            match self.poll_once(device, stop, log) {
                Ok(()) => return,
                Err(error) => log(&format!("{}: {error}; trying again in {} s", device.address, wait.as_secs())),
            }
            pause(wait, stop);
            wait = (wait * 2).min(LONGEST_PAUSE);
        }
    }

    /// Polls every device on its own thread until `stop` answers true.
    pub fn serve(&self, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync))
    where
        S: Sync,
    {
        std::thread::scope(|scope| {
            for device in &self.devices {
                scope.spawn(move || self.follow(device, stop, log));
            }
        });
    }
}
