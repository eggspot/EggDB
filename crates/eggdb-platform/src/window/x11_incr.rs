//! The `INCR` mechanism of the Inter-Client Communication Conventions Manual (ICCCM, section
//! 2.7.2, "INCR Properties"), which moves a selection larger than one X11 request in pieces,
//! from the public specification: the owner answers with a property of type `INCR` holding a
//! lower bound of the size, then writes one chunk each time the requestor deletes the property,
//! and ends with a chunk of length zero; the requestor deletes the property each time it has
//! read a chunk. Both sides are kept here as small state machines without I/O, so the window
//! code only moves their chunks and the protocol is tested without a server.

use std::ops::Range;
use eggdb_platform_core::time::Instant;
use std::time::Duration;

use super::WindowError;

/// The longest a transfer waits for its other side before it is given up.
pub const INCR_STALL_LIMIT: Duration = Duration::from_secs(10);
/// The most bytes one chunk carries, below the largest request every server takes.
pub const MAX_INCR_CHUNK: usize = 256 << 10;
/// The most transfers one window serves at the same time; another requestor is refused until
/// one ends.
pub const MAX_OUTGOING_TRANSFERS: usize = 8;
/// The longest a requestor reads one selection in increments, from the `INCR` answer to the
/// empty chunk, however often chunks arrive.
pub const INCR_READ_LIMIT: Duration = Duration::from_secs(30);
/// The most times a requestor told of a new value finds the property gone before it gives the
/// transfer up.
pub const MOST_GONE_CHUNKS: u32 = 8;

/// The bytes a chunk of a selection may carry for a server whose largest request is
/// `maximum_request_length` 4-byte units, after `header` bytes of `ChangeProperty`: at most
/// [`MAX_INCR_CHUNK`] and never zero.
pub fn incr_chunk_size(maximum_request_length: u16, header: usize) -> usize {
    usize::from(maximum_request_length)
        .saturating_mul(4)
        .saturating_sub(header)
        .clamp(1, MAX_INCR_CHUNK)
}

/// The size an `INCR` property's value announces (one 32-bit value in the client's byte order,
/// little-endian here), `None` when the value is not one.
pub fn incr_lower_bound(value: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = value.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// What an owner writes next into the requestor's property, after the requestor deleted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncrStep {
    /// These bytes of the selection, as the next chunk.
    Chunk(Range<usize>),
    /// The chunk of length zero that ends the transfer.
    End,
    /// The transfer is over: the requestor deleted the empty chunk too.
    Done,
}

/// A selection an owner sends a requestor in increments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingIncr {
    /// The window that asked for the selection.
    pub requestor: u32,
    /// The requestor's property the chunks go into.
    pub property: u32,
    /// The type each chunk is written as, such as `UTF8_STRING`.
    pub target: u32,
    bytes: Vec<u8>,
    chunk: usize,
    sent: usize,
    ended: bool,
    touched: Instant,
}

impl OutgoingIncr {
    /// A transfer of `bytes` in chunks of at most `chunk` bytes, started at `now`.
    pub fn new(
        requestor: u32,
        property: u32,
        target: u32,
        bytes: Vec<u8>,
        chunk: usize,
        now: Instant,
    ) -> Self {
        Self {
            requestor,
            property,
            target,
            bytes,
            chunk: chunk.max(1),
            sent: 0,
            ended: false,
            touched: now,
        }
    }

    /// The value of the `INCR` property that starts the transfer: the selection's size, or
    /// the largest 32-bit value for a larger one (it is a lower bound).
    pub fn announced_size(&self) -> u32 {
        u32::try_from(self.bytes.len()).unwrap_or(u32::MAX)
    }

    /// Whether a deletion of `property` on `window` belongs to this transfer.
    pub fn is_for(&self, window: u32, property: u32) -> bool {
        self.requestor == window && self.property == property
    }

    /// The bytes of a [`IncrStep::Chunk`].
    pub fn bytes(&self, range: Range<usize>) -> &[u8] {
        self.bytes.get(range).unwrap_or(&[])
    }

    /// The requestor deleted the property at `now`: the next chunk, then the empty one, then
    /// [`IncrStep::Done`].
    pub fn next(&mut self, now: Instant) -> IncrStep {
        self.touched = now;
        if self.sent < self.bytes.len() {
            let end = self.sent.saturating_add(self.chunk).min(self.bytes.len());
            let range = self.sent..end;
            self.sent = end;
            return IncrStep::Chunk(range);
        }
        if self.ended {
            return IncrStep::Done;
        }
        self.ended = true;
        IncrStep::End
    }

    /// Whether the requestor has not deleted the property for [`INCR_STALL_LIMIT`].
    pub fn is_stalled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.touched) >= INCR_STALL_LIMIT
    }

    /// When the transfer stalls unless the requestor deletes the property before.
    pub fn stalls_at(&self) -> Instant {
        self.touched
            .checked_add(INCR_STALL_LIMIT)
            .unwrap_or(self.touched)
    }
}

/// Where a requestor's transfer is after a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrProgress {
    /// More chunks follow.
    More,
    /// The empty chunk came: the selection is complete.
    Done,
}

/// A selection a requestor reads in increments, at most `limit` bytes and for at most
/// [`INCR_READ_LIMIT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingIncr {
    bytes: Vec<u8>,
    limit: usize,
    gives_up_at: Instant,
    gone: u32,
}

impl IncomingIncr {
    /// A transfer started at `now` whose `INCR` property announced `announced` bytes at least;
    /// refused when that is already more than `limit`.
    pub fn new(announced: Option<u32>, limit: usize, now: Instant) -> Result<Self, WindowError> {
        let announced = announced.map_or(0, |size| usize::try_from(size).unwrap_or(usize::MAX));
        if announced > limit {
            return Err(too_large(limit));
        }
        Ok(Self {
            bytes: Vec::with_capacity(announced.min(MAX_INCR_CHUNK)),
            limit,
            gives_up_at: now.checked_add(INCR_READ_LIMIT).unwrap_or(now),
            gone: 0,
        })
    }

    /// Until when to wait for the next chunk at `now`: `chunk_wait` from now, but never past
    /// the transfer's own [`INCR_READ_LIMIT`], which refuses it once it has passed.
    pub fn chunk_deadline(
        &self,
        now: Instant,
        chunk_wait: Duration,
    ) -> Result<Instant, WindowError> {
        if now >= self.gives_up_at {
            return Err(WindowError::Io(format!(
                "the clipboard's owner took more than {} s to send its text",
                INCR_READ_LIMIT.as_secs()
            )));
        }
        Ok(now
            .checked_add(chunk_wait)
            .map_or(self.gives_up_at, |at| at.min(self.gives_up_at)))
    }

    /// The property was gone when read after a new value was announced; refused after
    /// [`MOST_GONE_CHUNKS`] such reads.
    pub fn property_gone(&mut self) -> Result<(), WindowError> {
        self.gone = self.gone.saturating_add(1);
        if self.gone > MOST_GONE_CHUNKS {
            return Err(WindowError::Protocol(
                "the clipboard's owner kept removing its text before it was read".to_owned(),
            ));
        }
        Ok(())
    }

    /// Adds the next chunk: an empty one ends the transfer, and one that takes the selection
    /// past the limit is refused.
    pub fn take(&mut self, chunk: &[u8]) -> Result<IncrProgress, WindowError> {
        if chunk.is_empty() {
            return Ok(IncrProgress::Done);
        }
        if self.bytes.len().saturating_add(chunk.len()) > self.limit {
            return Err(too_large(self.limit));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(IncrProgress::More)
    }

    /// The selection read so far, as text (bytes that are not UTF-8 become U+FFFD).
    pub fn into_text(self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn too_large(limit: usize) -> WindowError {
    WindowError::InvalidInput(format!(
        "the clipboard holds more than {limit} bytes of text"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_fit_the_servers_largest_request_and_are_never_empty() {
        assert_eq!(incr_chunk_size(65535, 24), 65535 * 4 - 24);
        assert_eq!(incr_chunk_size(4096, 24), 4096 * 4 - 24);
        assert_eq!(incr_chunk_size(0, 24), 1);
        assert_eq!(incr_lower_bound(&70_000u32.to_le_bytes()), Some(70_000));
        assert_eq!(incr_lower_bound(&[1, 2]), None);
    }

    #[test]
    fn an_owner_sends_each_chunk_after_a_deletion_then_an_empty_one() {
        let now = Instant::now();
        let mut transfer = OutgoingIncr::new(7, 8, 9, b"abcdefghij".to_vec(), 4, now);
        assert_eq!(transfer.announced_size(), 10);
        assert!(transfer.is_for(7, 8));
        assert!(!transfer.is_for(7, 9));
        let mut chunks = Vec::new();
        loop {
            match transfer.next(now) {
                IncrStep::Chunk(range) => chunks.push(transfer.bytes(range).to_vec()),
                IncrStep::End => chunks.push(Vec::new()),
                IncrStep::Done => break,
            }
        }
        assert_eq!(
            chunks,
            vec![
                b"abcd".to_vec(),
                b"efgh".to_vec(),
                b"ij".to_vec(),
                Vec::new()
            ]
        );
        assert_eq!(transfer.next(now), IncrStep::Done);
    }

    #[test]
    fn an_empty_selection_sends_only_the_ending_chunk() {
        let now = Instant::now();
        let mut transfer = OutgoingIncr::new(1, 2, 3, Vec::new(), 0, now);
        assert_eq!(transfer.next(now), IncrStep::End);
        assert_eq!(transfer.next(now), IncrStep::Done);
    }

    #[test]
    fn a_transfer_the_requestor_stops_deleting_stalls() {
        let now = Instant::now();
        let mut transfer = OutgoingIncr::new(1, 2, 3, vec![0; 10], 4, now);
        assert!(!transfer.is_stalled(now + Duration::from_secs(9)));
        assert!(transfer.is_stalled(now + INCR_STALL_LIMIT));
        transfer.next(now + Duration::from_secs(9));
        assert!(!transfer.is_stalled(now + INCR_STALL_LIMIT));
    }

    #[test]
    fn a_requestor_joins_chunks_until_the_empty_one_and_keeps_to_its_limit() {
        let now = Instant::now();
        let mut incoming = IncomingIncr::new(Some(6), 10, now).unwrap();
        assert_eq!(incoming.take(b"caf"), Ok(IncrProgress::More));
        assert_eq!(incoming.take("\u{e9}!".as_bytes()), Ok(IncrProgress::More));
        assert_eq!(incoming.take(b""), Ok(IncrProgress::Done));
        assert_eq!(incoming.into_text(), "caf\u{e9}!");
        assert!(IncomingIncr::new(Some(11), 10, now).is_err());
        let mut small = IncomingIncr::new(None, 4, now).unwrap();
        assert_eq!(small.take(b"abcd"), Ok(IncrProgress::More));
        assert!(small.take(b"e").is_err());
        let mut lossy = IncomingIncr::new(None, 4, now).unwrap();
        assert_eq!(lossy.take(&[0xff, b'a']), Ok(IncrProgress::More));
        assert_eq!(lossy.into_text(), "\u{fffd}a");
    }

    #[test]
    fn a_stalled_transfer_says_when_it_stalls() {
        let now = Instant::now();
        let mut transfer = OutgoingIncr::new(1, 2, 3, vec![0; 10], 4, now);
        assert_eq!(transfer.stalls_at(), now + INCR_STALL_LIMIT);
        transfer.next(now + Duration::from_secs(3));
        assert_eq!(
            transfer.stalls_at(),
            now + Duration::from_secs(3) + INCR_STALL_LIMIT
        );
    }

    #[test]
    fn a_requestor_gives_up_a_dripping_transfer_at_its_overall_limit() {
        let now = Instant::now();
        let mut incoming = IncomingIncr::new(None, 1 << 20, now).unwrap();
        let wait = Duration::from_secs(2);
        assert_eq!(incoming.chunk_deadline(now, wait), Ok(now + wait));
        let mut at = now;
        while at < now + INCR_READ_LIMIT {
            assert_eq!(incoming.take(b"a"), Ok(IncrProgress::More));
            let deadline = incoming.chunk_deadline(at, wait).unwrap();
            assert!(deadline <= now + INCR_READ_LIMIT);
            at += Duration::from_millis(1900);
        }
        assert!(incoming.chunk_deadline(at, wait).is_err());
        assert_eq!(
            incoming.chunk_deadline(now + INCR_READ_LIMIT - Duration::from_secs(1), wait),
            Ok(now + INCR_READ_LIMIT)
        );
    }

    #[test]
    fn a_requestor_gives_up_an_owner_that_keeps_removing_the_property() {
        let mut incoming = IncomingIncr::new(None, 10, Instant::now()).unwrap();
        for _ in 0..MOST_GONE_CHUNKS {
            assert_eq!(incoming.property_gone(), Ok(()));
        }
        assert!(incoming.property_gone().is_err());
    }
}
