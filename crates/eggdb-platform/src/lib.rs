//! EggDB's platform layer (D8).
//!
//! Every piece of the engine that touches the operating system or the hardware goes through the
//! traits in this crate, so the engine never calls an OS API itself:
//! - [`FileSystem`] and [`File`]: positional reads and writes, durability barriers and the
//!   namespace operations a write-ahead log and a page store need;
//! - [`Clock`]: a monotonic clock for timeouts and a wall clock for hybrid logical clock
//!   timestamps (D20).
//!
//! Each trait has an implementation backed by the operating system ([`OsFileSystem`],
//! [`SystemClock`]) and a deterministic simulation ([`SimFileSystem`], [`SimClock`]) that tests
//! use to inject faults and crashes reproducibly from a seed (D9).

mod clock;
mod error;
mod fs;
mod os_fs;
mod rng;
mod sim_fs;

pub use clock::{Clock, SimClock, SystemClock};
pub use error::{Error, ErrorKind, Result};
pub use fs::{File, FileSystem, OpenMode};
pub use os_fs::{OsFile, OsFileSystem};
pub use sim_fs::{CrashMode, SimFile, SimFileSystem, TORN_WRITE_SECTOR_SIZE};
