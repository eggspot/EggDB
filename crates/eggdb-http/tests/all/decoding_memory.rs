use eggdb_content::crc32;
use eggdb_http::{Limits, Request, Response, serve_with_limits};
use eggdb_platform::connection_limits::MemoryBudget;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const BUDGET_BYTES: usize = 4 << 20;
const MAX_BODY_BYTES: usize = 8 << 20;
const CONCURRENT_BOMBS: usize = 8;
const ALLOWANCE_PER_CONNECTION: usize = 256 << 10;
const COPIES_PAST_THE_BODY_LIMIT: usize = 40_000;
const COPIES_PAST_THE_BUDGET_ONLY: usize = 24_000;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

struct PeakCounting;

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl PeakCounting {
    fn grew(bytes: usize) {
        let live = LIVE_BYTES.fetch_add(bytes, Ordering::AcqRel) + bytes;
        PEAK_LIVE_BYTES.fetch_max(live, Ordering::AcqRel);
    }

    fn shrank(bytes: usize) {
        LIVE_BYTES.fetch_sub(bytes, Ordering::AcqRel);
    }

    fn peak_from_now() -> usize {
        let live = LIVE_BYTES.load(Ordering::Acquire);
        PEAK_LIVE_BYTES.store(live, Ordering::Release);
        live
    }
}

// SAFETY: every call goes unchanged to the system allocator, which meets `GlobalAlloc`'s
// contract; counting the bytes live changes nothing about the memory.
unsafe impl GlobalAlloc for PeakCounting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller meets `alloc`'s contract, which `System.alloc` shares.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            Self::grew(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller meets `alloc_zeroed`'s contract, which `System.alloc_zeroed` shares.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            Self::grew(layout.size());
        }
        block
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller meets `realloc`'s contract, and `ptr` came from this allocator,
        // which is the system allocator underneath.
        let block = unsafe { System.realloc(ptr, layout, new_size) };
        if !block.is_null() {
            if new_size > layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                Self::shrank(layout.size() - new_size);
            }
        }
        block
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Self::shrank(layout.size());
        // SAFETY: the caller meets `dealloc`'s contract, and `ptr` came from this allocator,
        // which is the system allocator underneath.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: PeakCounting = PeakCounting;

struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn bit(&mut self, set: bool) {
        if self.bits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if set {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.bits % 8);
        }
        self.bits += 1;
    }

    fn huffman_code(&mut self, code: u32, length: u32) {
        for at in (0..length).rev() {
            self.bit((code >> at) & 1 == 1);
        }
    }
}

fn zeros_gzip(copies: usize) -> Vec<u8> {
    let mut deflated = BitWriter {
        bytes: Vec::new(),
        bits: 0,
    };
    deflated.bit(true);
    deflated.bit(true);
    deflated.bit(false);
    deflated.huffman_code(0x30, 8);
    for _ in 0..copies {
        deflated.huffman_code(0b1100_0101, 8);
        deflated.huffman_code(0, 5);
    }
    deflated.huffman_code(0, 7);
    let plain = vec![0u8; 1 + 258 * copies];
    let mut member = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    member.extend_from_slice(&deflated.bytes);
    member.extend_from_slice(&crc32(&plain).to_le_bytes());
    member.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    member
}

fn status_of_gzip_post(address: SocketAddr, body: &[u8]) -> u16 {
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let head = format!(
        "POST /bulk HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = client.write_all(head.as_bytes());
    let _ = client.write_all(body);
    let mut answer = Vec::new();
    let _ = client.read_to_end(&mut answer);
    String::from_utf8_lossy(&answer)
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn accepted(_: &Request) -> Response {
    Response::text(200, "decoded")
}

#[test]
fn concurrent_gzip_bombs_decode_within_the_memory_budget() {
    let past_the_body_limit = zeros_gzip(COPIES_PAST_THE_BODY_LIMIT);
    let past_the_budget_only = zeros_gzip(COPIES_PAST_THE_BUDGET_ONLY);
    let budget = MemoryBudget::new(BUDGET_BYTES);
    let limits = Limits::new(MAX_BODY_BYTES, &budget).decoding_content_encoding();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    let start = Barrier::new(CONCURRENT_BOMBS);
    let (statuses, peak_growth) = std::thread::scope(|scope| {
        scope.spawn(|| {
            serve_with_limits(
                &listener,
                &accepted,
                &|| stopped.load(Ordering::Acquire),
                &limits,
            )
            .unwrap();
        });
        let stop_even_on_a_failure = StopOnDrop(&stopped);
        let before = PeakCounting::peak_from_now();
        let clients: Vec<_> = (0..CONCURRENT_BOMBS)
            .map(|client| {
                let bomb = if client % 2 == 0 {
                    &past_the_body_limit
                } else {
                    &past_the_budget_only
                };
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    status_of_gzip_post(address, bomb)
                })
            })
            .collect();
        let statuses: Vec<u16> = clients
            .into_iter()
            .map(|client| client.join().unwrap())
            .collect();
        let peak_growth = PEAK_LIVE_BYTES
            .load(Ordering::Acquire)
            .saturating_sub(before);
        drop(stop_even_on_a_failure);
        (statuses, peak_growth)
    });
    assert!(
        statuses.iter().all(|status| [413, 503].contains(status)),
        "{statuses:?}"
    );
    assert!(
        peak_growth <= BUDGET_BYTES + CONCURRENT_BOMBS * ALLOWANCE_PER_CONNECTION,
        "the bombs took {peak_growth} bytes at once under a budget of {BUDGET_BYTES}"
    );
    assert_eq!(budget.used_bytes(), 0);
}
