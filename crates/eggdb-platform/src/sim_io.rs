use crate::aligned::AlignedBuffer;
use crate::error::Result;
use crate::fs::File;
use crate::io::{IoCompletion, IoOp, IoQueue, IoQueueOptions, RejectedOp, check_reap, queue_full};
use crate::rng::SplitMix64;
use crate::sim_fs::SimFile;
use std::collections::VecDeque;

/// Asynchronous file I/O for deterministic tests (D9), made by
/// [`SimFileSystem::io_queue`](crate::FileSystem::io_queue).
///
/// It runs no threads. Each operation is pending until the simulation performs it on its
/// [`SimFile`], then done until it is reaped. Every [`IoQueue::submit`] and [`IoQueue::reap`]
/// performs a random number of pending operations in a random order, and `reap` performs more
/// until `min` are done, so engine code meets every completion order. All of it is drawn from a
/// generator seeded by [`SimFileSystem::seed_io_queues`](crate::SimFileSystem::seed_io_queues)
/// and the queue's creation number, so a run repeats exactly.
///
/// Operations go through the [`SimFile`] methods, so they are numbered and fail on purpose like
/// any other operation of the file system. Across [`SimFileSystem::crash`](crate::SimFileSystem::crash),
/// a done write is an unsynced change that the crash mode keeps or loses, a done sync made its
/// writes durable even though nobody reaped it, and a pending operation never happened: it is
/// performed after the crash and completes with [`ErrorKind::StaleHandle`](crate::ErrorKind::StaleHandle).
/// Dropping the queue performs the pending operations, as an operating system finishes them.
/// The thread count of [`IoQueueOptions`] is ignored.
#[derive(Debug)]
pub struct SimIoQueue {
    capacity: usize,
    pending: Vec<Pending>,
    done: VecDeque<IoCompletion<SimFile>>,
    rng: SplitMix64,
    short_transfers_per_million: u32,
}

#[derive(Debug)]
struct Pending {
    op: IoOp<SimFile>,
    user_data: u64,
}

impl SimIoQueue {
    pub(crate) fn new(options: IoQueueOptions, seed: u64) -> Result<Self> {
        let (capacity, _) = options.validated()?;
        Ok(Self {
            capacity,
            pending: Vec::with_capacity(capacity),
            done: VecDeque::with_capacity(capacity),
            rng: SplitMix64::new(seed),
            short_transfers_per_million: 0,
        })
    }

    /// Makes each later read and write transfer fewer bytes than its buffer holds with
    /// probability `per_million` in a million, as the operating system may, so code that
    /// resubmits the rest is exercised. A short transfer on an uncached handle stays a
    /// multiple of [`AlignedBuffer::ALIGNMENT`]. A rate of zero stops them.
    pub fn short_transfers(&mut self, per_million: u32) {
        self.short_transfers_per_million = per_million;
    }

    fn perform_some(&mut self) {
        let count = self.draw_below(self.pending.len() + 1);
        for _ in 0..count {
            self.perform_one();
        }
    }

    fn perform_one(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let index = self.draw_below(self.pending.len());
        let mut pending = self.pending.swap_remove(index);
        let limit = self.transfer_limit(&pending.op);
        let result = pending.op.perform_up_to(limit);
        self.done.push_back(IoCompletion {
            user_data: pending.user_data,
            result,
            op: pending.op,
        });
    }

    fn transfer_limit(&mut self, op: &IoOp<SimFile>) -> usize {
        let Some(buffer) = op.buffer() else {
            return usize::MAX;
        };
        if self.short_transfers_per_million == 0
            || !self.rng.one_in_million(self.short_transfers_per_million)
        {
            return usize::MAX;
        }
        let unit = if op.file().is_uncached() {
            AlignedBuffer::ALIGNMENT
        } else {
            1
        };
        let units = buffer.len() / unit;
        if units < 2 {
            return usize::MAX;
        }
        (1 + self.draw_below(units - 1)) * unit
    }

    fn draw_below(&mut self, bound: usize) -> usize {
        match u64::try_from(bound) {
            Ok(0) | Err(_) => 0,
            Ok(bound) => usize::try_from(self.rng.next_u64() % bound).unwrap_or(0),
        }
    }
}

impl IoQueue for SimIoQueue {
    type File = SimFile;

    fn submit(
        &mut self,
        op: IoOp<SimFile>,
        user_data: u64,
    ) -> core::result::Result<(), RejectedOp<SimFile>> {
        if self.in_flight() >= self.capacity {
            return Err(queue_full(op));
        }
        self.pending.push(Pending { op, user_data });
        self.perform_some();
        Ok(())
    }

    fn reap(&mut self, completions: &mut Vec<IoCompletion<SimFile>>, min: usize) -> Result<usize> {
        check_reap(min, self.in_flight())?;
        self.perform_some();
        while self.done.len() < min {
            self.perform_one();
        }
        let reaped = self.done.len();
        completions.extend(self.done.drain(..));
        Ok(reaped)
    }

    fn in_flight(&self) -> usize {
        self.pending.len() + self.done.len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Drop for SimIoQueue {
    fn drop(&mut self) {
        while !self.pending.is_empty() {
            self.perform_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SimIoQueue;
    use crate::{
        AlignedBuffer, CrashMode, ErrorKind, File, FileSystem, IoCompletion, IoOp, IoQueue,
        IoQueueOptions, OpenMode, SimFile, SimFileSystem,
    };
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Arc;

    fn file(fs: &SimFileSystem, name: &str) -> Arc<SimFile> {
        Arc::new(fs.open(Path::new(name), OpenMode::Create).unwrap())
    }

    fn queue(fs: &SimFileSystem) -> SimIoQueue {
        fs.io_queue(IoQueueOptions::new()).unwrap()
    }

    fn submit(queue: &mut SimIoQueue, op: IoOp<SimFile>, user_data: u64) {
        assert!(queue.submit(op, user_data).is_ok());
    }

    fn reap_all(queue: &mut SimIoQueue) -> Vec<IoCompletion<SimFile>> {
        let mut completions = Vec::new();
        let in_flight = queue.in_flight();
        queue.reap(&mut completions, in_flight).unwrap();
        completions
    }

    fn write(file: &Arc<SimFile>, offset: u64, bytes: &[u8]) -> IoOp<SimFile> {
        IoOp::Write {
            file: Arc::clone(file),
            offset,
            buffer: bytes.to_vec().into(),
        }
    }

    fn completion_order(seed: u64) -> Vec<u64> {
        let fs = SimFileSystem::new();
        fs.seed_io_queues(seed);
        let data = file(&fs, "data");
        let mut queue = queue(&fs);
        for tag in 0..3 {
            submit(&mut queue, write(&data, tag, &[1]), tag);
        }
        reap_all(&mut queue)
            .iter()
            .map(|completion| completion.user_data)
            .collect()
    }

    #[test]
    fn a_seed_repeats_its_completion_order_and_seeds_reach_every_order() {
        assert_eq!(completion_order(42), completion_order(42));
        let orders: BTreeSet<Vec<u64>> = (0..1000).map(completion_order).collect();
        assert_eq!(orders.len(), 6);
    }

    #[test]
    fn queues_of_one_run_draw_different_orders() {
        let fs = SimFileSystem::new();
        let data = file(&fs, "data");
        let orders: BTreeSet<Vec<u64>> = (0..16)
            .map(|_| {
                let mut queue = queue(&fs);
                for tag in 0..4 {
                    submit(&mut queue, write(&data, tag, &[1]), tag);
                }
                reap_all(&mut queue)
                    .iter()
                    .map(|completion| completion.user_data)
                    .collect()
            })
            .collect();
        assert!(orders.len() > 1);
    }

    #[test]
    fn a_sync_in_flight_at_a_crash_either_happened_or_never_did() {
        let mut outcomes = BTreeSet::new();
        for seed in 0..64 {
            let fs = SimFileSystem::new();
            fs.seed_io_queues(seed);
            let data = file(&fs, "data");
            fs.sync_dir(Path::new("")).unwrap();
            let mut queue = queue(&fs);
            submit(&mut queue, write(&data, 0, b"logged"), 0);
            assert!(reap_all(&mut queue)[0].result.is_ok());
            submit(
                &mut queue,
                IoOp::SyncData {
                    file: Arc::clone(&data),
                },
                1,
            );
            fs.crash(CrashMode::DropUnsynced);
            let synced = match reap_all(&mut queue).pop().unwrap().result {
                Ok(_) => true,
                Err(error) => {
                    assert_eq!(error.kind(), ErrorKind::StaleHandle);
                    false
                }
            };
            let reopened = fs.open(Path::new("data"), OpenMode::Read).unwrap();
            assert_eq!(reopened.size().unwrap() == 6, synced);
            outcomes.insert(synced);
        }
        assert_eq!(outcomes.len(), 2);
    }

    #[test]
    fn short_transfers_happen_and_keep_uncached_alignment() {
        let fs = SimFileSystem::new();
        let page = AlignedBuffer::ALIGNMENT;
        let cached = file(&fs, "cached");
        let uncached = Arc::new(
            fs.open_uncached(Path::new("uncached"), OpenMode::Create)
                .unwrap(),
        );
        let mut queue = queue(&fs);
        queue.short_transfers(500_000);
        let mut cached_short = 0;
        let mut uncached_short = 0;
        for _ in 0..64 {
            submit(&mut queue, write(&cached, 0, &[3; 64]), 0);
            let buffer = AlignedBuffer::zeroed(4 * page).unwrap();
            submit(
                &mut queue,
                IoOp::Write {
                    file: Arc::clone(&uncached),
                    offset: 0,
                    buffer: buffer.into(),
                },
                1,
            );
            for completion in reap_all(&mut queue) {
                let written = completion.result.unwrap();
                assert!(written > 0);
                if completion.user_data == 0 {
                    cached_short += usize::from(written < 64);
                } else {
                    assert!(written.is_multiple_of(page));
                    uncached_short += usize::from(written < 4 * page);
                }
            }
        }
        assert!(cached_short > 0 && uncached_short > 0);
        queue.short_transfers(0);
        submit(&mut queue, write(&cached, 0, &[3; 64]), 0);
        assert_eq!(reap_all(&mut queue)[0].result.as_ref().unwrap(), &64);
    }

    #[test]
    fn injected_faults_arrive_in_completions_and_change_nothing() {
        let fs = SimFileSystem::new();
        let data = file(&fs, "data");
        let mut queue = queue(&fs);
        fs.fail_operation(fs.operation_count());
        submit(&mut queue, write(&data, 0, b"lost"), 0);
        let completion = reap_all(&mut queue).pop().unwrap();
        assert_eq!(
            completion.result.unwrap_err().kind(),
            ErrorKind::InjectedFault
        );
        assert_eq!(data.size().unwrap(), 0);
    }
}
