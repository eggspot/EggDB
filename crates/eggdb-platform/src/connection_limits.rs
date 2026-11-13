//! What a network server limits so that no client, and no other program on the same machine,
//! can lock other users out or exhaust the server's memory (D9, D22, D254):
//! - [`LoginSlots`]: the connections still logging in. When every slot is taken, a new
//!   connection takes the slot of the oldest one that has waited at least
//!   [`LoginSlots::eviction_age`], and that one is dropped; a peer on another machine holds at
//!   most [`MAX_LOGINS_IN_PROGRESS_PER_REMOTE_PEER`] of them.
//! - [`MemoryBudget`]: the bytes every connection of a server holds in buffered messages,
//!   prepared statements and bound parameters together, so many connections cannot each hold
//!   their largest message at once.
//! - The default timeouts every protocol server shares: [`DEFAULT_AUTHENTICATION_TIMEOUT`] and
//!   [`DEFAULT_IDLE_TIMEOUT`].

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use eggdb_platform_core::time::Instant;
use std::time::Duration;

/// How long a client has from connecting to finishing its login, unless a server is told
/// otherwise.
pub const DEFAULT_AUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a logged-in connection may wait between requests before the server closes it,
/// unless a server is told otherwise.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// The most connections a server lets log in at once.
pub const MAX_LOGINS_IN_PROGRESS: usize = 16;
/// The most of [`MAX_LOGINS_IN_PROGRESS`] that one address on another machine may hold; an
/// IPv6 address counts by its `/64` network.
pub const MAX_LOGINS_IN_PROGRESS_PER_REMOTE_PEER: usize = MAX_LOGINS_IN_PROGRESS / 2;
/// How long a login waits before a newer connection may take its slot.
pub const LOGIN_EVICTION_AGE: Duration = Duration::from_secs(1);
/// The bytes a server's connections may hold together unless it is told otherwise: 4 GiB, or
/// all of memory on a 32-bit machine.
pub const DEFAULT_MEMORY_BUDGET_BYTES: usize = 4usize.saturating_mul(1 << 30);

/// The bytes a request or reply may take without holding them against the budget: a small
/// one costs less to serve than to count, and a connection's own buffers are of this size.
pub const UNCOUNTED_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peer {
    ThisMachine,
    Remote(IpAddr),
}

impl Peer {
    fn of(address: Option<IpAddr>) -> Self {
        match address {
            None => Self::ThisMachine,
            Some(address) if address.is_loopback() => Self::ThisMachine,
            Some(IpAddr::V6(address)) => match address.to_ipv4_mapped() {
                Some(mapped) if mapped.is_loopback() => Self::ThisMachine,
                Some(mapped) => Self::Remote(IpAddr::V4(mapped)),
                None => {
                    let network = u128::from(address) & (u128::MAX << 64);
                    Self::Remote(IpAddr::V6(network.into()))
                }
            },
            Some(address) => Self::Remote(address),
        }
    }
}

#[derive(Debug)]
struct PendingLogin {
    id: u64,
    peer: Peer,
    started: Instant,
    evicted: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct Pending {
    next_id: u64,
    oldest_first: VecDeque<PendingLogin>,
}

/// The slots of the connections still logging in to one server.
#[derive(Debug)]
pub struct LoginSlots {
    total: usize,
    per_remote_peer: usize,
    eviction_age: Duration,
    pending: Mutex<Pending>,
}

impl Default for LoginSlots {
    fn default() -> Self {
        Self::new(
            MAX_LOGINS_IN_PROGRESS,
            MAX_LOGINS_IN_PROGRESS_PER_REMOTE_PEER,
            LOGIN_EVICTION_AGE,
        )
    }
}

impl LoginSlots {
    /// Slots for `total` logins at once, at most `per_remote_peer` of them from one address on
    /// another machine; a login may lose its slot to a newer one once it has waited
    /// `eviction_age`.
    pub fn new(total: usize, per_remote_peer: usize, eviction_age: Duration) -> Self {
        Self {
            total: total.max(1),
            per_remote_peer: per_remote_peer.clamp(1, total.max(1)),
            eviction_age,
            pending: Mutex::new(Pending::default()),
        }
    }

    /// How long a login waits before a newer connection may take its slot.
    pub fn eviction_age(&self) -> Duration {
        self.eviction_age
    }

    /// The logins holding a slot now.
    pub fn in_progress(&self) -> usize {
        self.lock().oldest_first.len()
    }

    /// A slot for a login from `peer` (its IP address; `None` for a local socket), or `None`
    /// when every slot it may take is held by a login younger than
    /// [`LoginSlots::eviction_age`]. When the slots are full, the oldest login old enough
    /// loses its slot: the oldest of the same peer's when that peer is on another machine and
    /// holds its share, otherwise the oldest of all. Its [`LoginTicket::is_evicted`] turns
    /// true and its server drops it.
    pub fn admit(&self, peer: Option<IpAddr>) -> Option<LoginTicket<'_>> {
        self.admit_at(peer, Instant::now())
    }

    fn admit_at(&self, peer: Option<IpAddr>, now: Instant) -> Option<LoginTicket<'_>> {
        let peer = Peer::of(peer);
        let mut pending = self.lock();
        let peer_count = pending
            .oldest_first
            .iter()
            .filter(|login| login.peer == peer)
            .count();
        let peer_is_full = peer != Peer::ThisMachine && peer_count >= self.per_remote_peer;
        if peer_is_full || pending.oldest_first.len() >= self.total {
            let victim = pending.oldest_first.iter().position(|login| {
                (!peer_is_full || login.peer == peer)
                    && now.saturating_duration_since(login.started) >= self.eviction_age
            })?;
            if let Some(evicted) = pending.oldest_first.remove(victim) {
                evicted.evicted.store(true, Ordering::Release);
            }
        }
        let id = pending.next_id;
        pending.next_id = pending.next_id.wrapping_add(1);
        let evicted = Arc::new(AtomicBool::new(false));
        pending.oldest_first.push_back(PendingLogin {
            id,
            peer,
            started: now,
            evicted: Arc::clone(&evicted),
        });
        Some(LoginTicket {
            slots: self,
            id,
            evicted,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release(&self, id: u64) {
        let mut pending = self.lock();
        if let Some(index) = pending.oldest_first.iter().position(|login| login.id == id) {
            pending.oldest_first.remove(index);
        }
    }
}

/// One login's slot, given back when dropped: once the login succeeds, or when the connection
/// ends.
#[derive(Debug)]
pub struct LoginTicket<'s> {
    slots: &'s LoginSlots,
    id: u64,
    evicted: Arc<AtomicBool>,
}

impl LoginTicket<'_> {
    /// Whether a newer connection took this login's slot, so the server should drop it.
    pub fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::Acquire)
    }
}

impl Drop for LoginTicket<'_> {
    fn drop(&mut self) {
        if !self.is_evicted() {
            self.slots.release(self.id);
        }
    }
}

/// The bytes a server's connections may hold together, shared by every protocol a server
/// speaks.
#[derive(Debug)]
pub struct MemoryBudget {
    limit_bytes: usize,
    used_bytes: AtomicUsize,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MEMORY_BUDGET_BYTES)
    }
}

impl MemoryBudget {
    /// A budget of `limit_bytes`.
    pub const fn new(limit_bytes: usize) -> Self {
        Self {
            limit_bytes,
            used_bytes: AtomicUsize::new(0),
        }
    }

    /// The bytes the connections may hold together.
    pub fn limit_bytes(&self) -> usize {
        self.limit_bytes
    }

    /// The bytes reserved now.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Acquire)
    }

    /// A reservation of no bytes, to grow with [`Reservation::resize`].
    pub fn empty_reservation(&self) -> Reservation<'_> {
        Reservation {
            budget: self,
            bytes: 0,
        }
    }

    /// A reservation of `bytes`, or `None` when the budget has less left.
    pub fn reserve(&self, bytes: usize) -> Option<Reservation<'_>> {
        let mut reservation = self.empty_reservation();
        reservation.resize(bytes).then_some(reservation)
    }

    /// A reservation of `bytes` that keeps the budget alive itself, so it can move to another
    /// thread with what it holds; `None` when the budget has less left.
    pub fn reserve_owned(budget: &Arc<Self>, bytes: usize) -> Option<OwnedReservation> {
        budget.take(bytes).then(|| OwnedReservation {
            budget: Arc::clone(budget),
            bytes,
        })
    }

    fn take(&self, bytes: usize) -> bool {
        self.used_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|&total| total <= self.limit_bytes)
            })
            .is_ok()
    }

    fn give_back(&self, bytes: usize) {
        let _ = self
            .used_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(bytes))
            });
    }
}

/// Bytes held against a [`MemoryBudget`], given back when dropped.
#[derive(Debug)]
pub struct Reservation<'b> {
    budget: &'b MemoryBudget,
    bytes: usize,
}

impl Reservation<'_> {
    /// The bytes reserved.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Makes the reservation `bytes` large. Shrinking always succeeds; growing fails, leaving
    /// the reservation as it was, when the budget has too little left.
    pub fn resize(&mut self, bytes: usize) -> bool {
        if bytes > self.bytes {
            if !self.budget.take(bytes - self.bytes) {
                return false;
            }
        } else {
            self.budget.give_back(self.bytes - bytes);
        }
        self.bytes = bytes;
        true
    }

    /// Makes the reservation `bytes` large when that is more than [`UNCOUNTED_BUFFER_BYTES`],
    /// and gives every byte back when it is not; false, leaving the reservation as it was,
    /// when the budget has too little left.
    pub fn hold_if_large(&mut self, bytes: usize) -> bool {
        if bytes <= UNCOUNTED_BUFFER_BYTES {
            self.resize(0)
        } else {
            self.resize(bytes)
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.budget.give_back(self.bytes);
    }
}

/// Bytes held against a shared [`MemoryBudget`], given back when dropped.
#[derive(Debug)]
pub struct OwnedReservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl OwnedReservation {
    /// The bytes reserved.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for OwnedReservation {
    fn drop(&mut self) {
        self.budget.give_back(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{LoginSlots, MemoryBudget, UNCOUNTED_BUFFER_BYTES};
    use std::net::IpAddr;
    use std::sync::Arc;
    use eggdb_platform_core::time::Instant;
    use std::time::Duration;

    fn address(text: &str) -> Option<IpAddr> {
        text.parse().ok()
    }

    #[test]
    fn a_full_set_of_logins_gives_the_oldest_slot_to_a_new_one() {
        let slots = LoginSlots::new(3, 2, Duration::ZERO);
        let start = Instant::now();
        let tickets: Vec<_> = (0..3u64)
            .map(|second| {
                slots
                    .admit_at(None, start + Duration::from_secs(second))
                    .unwrap()
            })
            .collect();
        let newest = slots
            .admit_at(None, start + Duration::from_secs(5))
            .unwrap();
        assert!(tickets[0].is_evicted());
        assert!(!tickets[1].is_evicted() && !tickets[2].is_evicted());
        assert!(!newest.is_evicted());
        assert_eq!(slots.in_progress(), 3);
        drop(tickets);
        assert_eq!(slots.in_progress(), 1);
    }

    #[test]
    fn a_login_younger_than_the_eviction_age_keeps_its_slot() {
        let slots = LoginSlots::new(2, 2, Duration::from_secs(1));
        let start = Instant::now();
        let first = slots.admit_at(None, start).unwrap();
        let second = slots.admit_at(None, start).unwrap();
        assert!(
            slots
                .admit_at(None, start + Duration::from_millis(500))
                .is_none()
        );
        assert!(!first.is_evicted() && !second.is_evicted());
        let third = slots
            .admit_at(None, start + Duration::from_secs(2))
            .unwrap();
        assert!(first.is_evicted());
        drop((second, third));
        assert_eq!(slots.in_progress(), 0);
    }

    #[test]
    fn a_remote_peer_at_its_share_only_evicts_its_own_logins() {
        let slots = LoginSlots::new(4, 2, Duration::ZERO);
        let start = Instant::now();
        let local = slots.admit_at(address("127.0.0.1"), start).unwrap();
        let mut remote: Vec<_> = (1..=2u64)
            .map(|second| {
                slots
                    .admit_at(address("203.0.113.9"), start + Duration::from_secs(second))
                    .unwrap()
            })
            .collect();
        remote.push(
            slots
                .admit_at(address("203.0.113.9"), start + Duration::from_secs(3))
                .unwrap(),
        );
        assert!(!local.is_evicted());
        assert!(remote[0].is_evicted());
        assert!(!remote[1].is_evicted() && !remote[2].is_evicted());
        assert_eq!(slots.in_progress(), 3);
    }

    #[test]
    fn ipv6_peers_count_by_their_64_bit_network_and_mapped_loopback_is_this_machine() {
        let slots = LoginSlots::new(8, 1, Duration::from_secs(60));
        let start = Instant::now();
        let first = slots.admit_at(address("2001:db8::1"), start).unwrap();
        assert!(slots.admit_at(address("2001:db8::2"), start).is_none());
        let other_network = slots.admit_at(address("2001:db8:0:1::1"), start);
        assert!(other_network.is_some());
        let mapped: Vec<_> = (0..3)
            .map(|_| slots.admit_at(address("::ffff:127.0.0.1"), start))
            .collect();
        assert!(mapped.iter().all(Option::is_some));
        assert!(!first.is_evicted());
    }

    #[test]
    fn this_machine_may_use_every_slot() {
        let slots = LoginSlots::new(4, 1, Duration::from_secs(60));
        let start = Instant::now();
        let tickets: Vec<_> = (0..4)
            .map(|_| slots.admit_at(address("::1"), start))
            .collect();
        assert!(tickets.iter().all(Option::is_some));
        assert!(slots.admit_at(None, start).is_none());
    }

    #[test]
    fn reservations_stop_at_the_limit_and_give_bytes_back() {
        let budget = MemoryBudget::new(100);
        let mut first = budget.reserve(60).unwrap();
        assert!(budget.reserve(41).is_none());
        let second = budget.reserve(40).unwrap();
        assert_eq!(budget.used_bytes(), 100);
        assert!(!first.resize(61));
        assert_eq!(first.bytes(), 60);
        assert!(first.resize(10));
        assert_eq!(budget.used_bytes(), 50);
        drop(second);
        assert_eq!(budget.used_bytes(), 10);
        drop(first);
        assert_eq!(budget.used_bytes(), 0);
        assert!(budget.reserve(usize::MAX).is_none());
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn only_buffers_over_the_uncounted_size_are_held() {
        let budget = MemoryBudget::new(4 * UNCOUNTED_BUFFER_BYTES);
        let mut held = budget.empty_reservation();
        assert!(held.hold_if_large(UNCOUNTED_BUFFER_BYTES));
        assert_eq!(budget.used_bytes(), 0);
        assert!(held.hold_if_large(3 * UNCOUNTED_BUFFER_BYTES));
        assert_eq!(budget.used_bytes(), 3 * UNCOUNTED_BUFFER_BYTES);
        assert!(!held.hold_if_large(5 * UNCOUNTED_BUFFER_BYTES));
        assert_eq!(held.bytes(), 3 * UNCOUNTED_BUFFER_BYTES);
        let mut other = budget.empty_reservation();
        assert!(!other.hold_if_large(2 * UNCOUNTED_BUFFER_BYTES));
        assert!(held.hold_if_large(10));
        assert_eq!(budget.used_bytes(), 0);
        assert!(other.hold_if_large(2 * UNCOUNTED_BUFFER_BYTES));
        drop(other);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn an_owned_reservation_gives_its_bytes_back_on_the_thread_that_drops_it() {
        let budget = Arc::new(MemoryBudget::new(100));
        let held = MemoryBudget::reserve_owned(&budget, 70).unwrap();
        assert_eq!(held.bytes(), 70);
        assert!(MemoryBudget::reserve_owned(&budget, 31).is_none());
        assert!(budget.reserve(31).is_none());
        std::thread::spawn(move || drop(held)).join().unwrap();
        assert_eq!(budget.used_bytes(), 0);
        assert!(MemoryBudget::reserve_owned(&budget, usize::MAX).is_none());
        assert_eq!(budget.used_bytes(), 0);
    }
}
