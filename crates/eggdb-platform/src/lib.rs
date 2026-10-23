//! EggDB's platform layer (D8).
//!
//! Every piece of the engine that touches the operating system or the hardware goes through the
//! traits in this crate, so the engine never calls an OS API itself:
//! - [`FileSystem`] and [`File`]: positional reads and writes, durability barriers and the
//!   namespace operations a write-ahead log and a page store need, and handles that bypass the
//!   operating system's cache, used with [`AlignedBuffer`];
//! - [`Clock`]: a monotonic clock for timeouts and a wall clock for hybrid logical clock
//!   timestamps (D20);
//! - [`Memory`]: the operating system's page sizes, and zeroed page-aligned [`MemoryRegion`]s
//!   on normal, huge or transparent huge pages ([`PageMode`]).
//!
//! Each trait has an implementation backed by the operating system ([`OsFileSystem`],
//! [`SystemClock`], [`OsMemory`]) and a deterministic simulation ([`SimFileSystem`],
//! [`SimClock`], [`SimMemory`]) that tests use to inject faults and crashes reproducibly from a
//! seed (D9).

mod aligned;
mod clock;
mod error;
mod fs;
mod memory;
mod os_fs;
mod os_memory;
mod rng;
mod sim_fs;
mod sim_memory;
mod uncached;

pub use aligned::AlignedBuffer;
pub use clock::{Clock, SimClock, SystemClock};
pub use error::{Error, ErrorKind, Result};
pub use fs::{File, FileSystem, OpenMode};
pub use memory::{Memory, MemoryRegion, PageMode};
pub use os_fs::{OsFile, OsFileSystem};
pub use os_memory::OsMemory;
pub use sim_fs::{CrashMode, SimFile, SimFileSystem};
pub use sim_memory::SimMemory;
