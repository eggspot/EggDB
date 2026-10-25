//! EggDB's platform layer (D8).
//!
//! Every piece of the engine that touches the operating system or the hardware goes through the
//! traits in this crate, so the engine never calls an OS API itself:
//! - [`FileSystem`] and [`File`]: positional reads and writes, durability barriers and the
//!   namespace operations a write-ahead log and a page store need, and handles that bypass the
//!   operating system's cache, used with [`AlignedBuffer`];
//! - [`IoQueue`]: asynchronous reads, writes and syncs of those files, submitted with a tag
//!   and reaped as [`IoCompletion`]s in any order, each [`IoOp`] owning its [`IoBuffer`] while
//!   it is in flight;
//! - [`Network`]: TCP listeners and connections, with a [`NetQueue`] of accepts, connects,
//!   receives and sends ([`NetOp`]) reaped as [`NetCompletion`]s in any order;
//! - [`Clock`]: a monotonic clock for timeouts and a wall clock for hybrid logical clock
//!   timestamps (D20);
//! - [`Memory`]: the operating system's page sizes, and zeroed page-aligned [`MemoryRegion`]s
//!   on normal, huge or transparent huge pages ([`PageMode`]);
//! - [`Cpus`]: the [`CpuSet`] a process may run on, how many threads can run at once, the
//!   machine's [`CpuTopology`] (cores, packages, NUMA nodes, shared caches), and pinning
//!   threads to CPUs; with [`ThreadOptions`] to start named, pinned threads and
//!   [`CachePadded`] against false sharing.
//!
//! Each trait has an implementation backed by the operating system ([`OsFileSystem`],
//! [`OsIoQueue`], [`OsNetwork`], [`SystemClock`], [`OsMemory`], [`OsCpus`]) and a
//! deterministic simulation ([`SimFileSystem`], [`SimIoQueue`], [`SimNetwork`], [`SimClock`],
//! [`SimMemory`], [`SimCpus`]) that tests use
//! to inject faults and crashes reproducibly from a seed (D9).

mod aligned;
mod cache;
mod clock;
mod cpus;
mod error;
mod fs;
mod io;
mod memory;
mod net;
mod os_cpus;
mod os_fs;
mod os_io;
mod os_io_uring;
mod os_iocp;
mod os_memory;
mod os_net;
mod rng;
mod sim_cpus;
mod sim_fs;
mod sim_io;
mod sim_memory;
mod sim_net;
mod threads;
mod topology;
#[cfg(any(test, target_os = "linux", target_os = "android"))]
mod topology_sysfs;
#[cfg(any(test, windows))]
mod topology_windows;
mod uncached;

pub use aligned::AlignedBuffer;
pub use cache::{CACHE_LINE_PADDING, CachePadded};
pub use clock::{Clock, SimClock, SystemClock};
pub use cpus::{CpuId, CpuSet, Cpus};
pub use error::{Error, ErrorKind, Result};
pub use fs::{File, FileSystem, OpenMode};
pub use io::{IoBackend, IoBuffer, IoCompletion, IoOp, IoQueue, IoQueueOptions, RejectedOp};
pub use memory::{Memory, MemoryRegion, PageMode};
pub use net::{
    Address, NetCompletion, NetListener, NetOp, NetOutput, NetQueue, NetQueueOptions, NetStream,
    Network, RejectedNetOp,
};
pub use os_cpus::OsCpus;
pub use os_fs::{OsFile, OsFileSystem};
pub use os_io::OsIoQueue;
pub use os_memory::OsMemory;
pub use os_net::{OsListener, OsNetQueue, OsNetwork, OsStream};
pub use sim_cpus::SimCpus;
pub use sim_fs::{CrashMode, SimFile, SimFileSystem};
pub use sim_io::SimIoQueue;
pub use sim_memory::SimMemory;
pub use sim_net::{SimHost, SimListener, SimNetQueue, SimNetwork, SimStream};
pub use threads::{SpawnedThread, ThreadOptions};
pub use topology::{
    CacheId, CoreId, CpuLocation, CpuTopology, NumaNodeId, PackageId, SiblingNumbering,
};
