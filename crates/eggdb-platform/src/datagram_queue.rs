use crate::error::{Error, ErrorKind, Result};
use crate::io::IoBuffer;
use crate::net::{DatagramSocket, NetQueueOptions};
use eggdb_platform_core::time::Instant;
use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const LONGEST_SINGLE_WAIT: Duration = Duration::from_millis(250);
const MOST_PER_BATCH: usize = 64;

/// One datagram operation on a [`DatagramQueue`], owning its buffer until it completes.
#[derive(Debug)]
#[non_exhaustive]
pub enum DatagramOp {
    /// Receives the next datagram into `buffer`, completing with
    /// [`DatagramOutput::Received`].
    Receive {
        /// The bytes to fill.
        buffer: IoBuffer,
    },
    /// Sends `buffer` as one datagram to `to`, completing with [`DatagramOutput::Sent`].
    Send {
        /// The datagram.
        buffer: IoBuffer,
        /// Where it goes.
        to: SocketAddr,
    },
}

impl DatagramOp {
    /// Gives back the operation's buffer.
    pub fn into_buffer(self) -> IoBuffer {
        match self {
            Self::Receive { buffer } | Self::Send { buffer, .. } => buffer,
        }
    }
}

/// What a successful datagram operation produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DatagramOutput {
    /// A receive's datagram: how many bytes of the buffer it filled, and who sent it.
    Received {
        /// The datagram's length, cut to the buffer's as [`DatagramSocket::receive_from`] cuts.
        length: usize,
        /// The sender.
        from: SocketAddr,
    },
    /// A send's byte count, always the whole datagram.
    Sent(usize),
}

/// A finished datagram operation: the caller's tag, the outcome and the operation.
#[derive(Debug)]
pub struct DatagramCompletion {
    /// The tag given to [`DatagramQueue::submit`].
    pub user_data: u64,
    /// What the operation produced, or why it failed.
    pub result: Result<DatagramOutput>,
    /// The operation, handing back its buffer.
    pub op: DatagramOp,
}

/// A datagram operation [`DatagramQueue::submit`] did not accept, handed back with the reason.
#[derive(Debug)]
pub struct RejectedDatagramOp {
    /// Why it was not accepted.
    pub error: Error,
    /// The operation, untouched.
    pub op: DatagramOp,
}

impl fmt::Display for RejectedDatagramOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "datagram operation not submitted: {}", self.error)
    }
}

impl std::error::Error for RejectedDatagramOp {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A completion queue of receives and sends on one UDP socket, owned by one thread, as a
/// [`NetQueue`](crate::NetQueue) is for connections: what QUIC runs on (D1114). Receives
/// complete in the order they were submitted, each with the next datagram, and a reap takes
/// every datagram already waiting in one [`DatagramSocket::receive_batch`], so a burst of
/// packets costs one system call on Linux. Sends go out when they are submitted, since a UDP
/// send never waits for the peer, and complete at the next reap.
///
/// It runs over any [`DatagramSocket`], the simulation's included, so a QUIC endpoint is tested
/// under the simulated network's loss, duplication and reordering.
pub struct DatagramQueue<D: DatagramSocket> {
    socket: Arc<D>,
    capacity: usize,
    receives: VecDeque<(u64, IoBuffer)>,
    finished: VecDeque<DatagramCompletion>,
}

impl<D: DatagramSocket> fmt::Debug for DatagramQueue<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatagramQueue")
            .field("socket", &self.socket)
            .field("capacity", &self.capacity)
            .field("receives", &self.receives.len())
            .field("finished", &self.finished.len())
            .finish()
    }
}

impl<D: DatagramSocket> DatagramQueue<D> {
    /// A queue on `socket` holding at most `options`' capacity of operations; refuses a
    /// capacity [`NetQueueOptions`] refuses with [`ErrorKind::InvalidInput`]. The options'
    /// backend is not used: every socket runs the same way.
    pub fn new(socket: Arc<D>, options: NetQueueOptions) -> Result<Self> {
        let capacity = options.validated()?;
        Ok(Self {
            socket,
            capacity,
            receives: VecDeque::new(),
            finished: VecDeque::new(),
        })
    }

    /// The socket the queue runs on.
    pub fn socket(&self) -> &Arc<D> {
        &self.socket
    }

    /// Starts `op`, tagged with `user_data`. Fails with [`ErrorKind::QueueFull`] when
    /// [`DatagramQueue::in_flight`] has reached [`DatagramQueue::capacity`], handing it back.
    /// A receive into an empty buffer completes with [`ErrorKind::InvalidInput`].
    pub fn submit(
        &mut self,
        op: DatagramOp,
        user_data: u64,
    ) -> core::result::Result<(), RejectedDatagramOp> {
        if self.in_flight() >= self.capacity {
            return Err(RejectedDatagramOp {
                error: Error::new(ErrorKind::QueueFull),
                op,
            });
        }
        match op {
            DatagramOp::Receive { buffer } if buffer.is_empty() => {
                self.finished.push_back(DatagramCompletion {
                    user_data,
                    result: Err(Error::new(ErrorKind::InvalidInput)),
                    op: DatagramOp::Receive { buffer },
                });
            }
            DatagramOp::Receive { buffer } => self.receives.push_back((user_data, buffer)),
            DatagramOp::Send { buffer, to } => {
                let result = self
                    .socket
                    .send_to(&buffer, to)
                    .map(DatagramOutput::Sent);
                self.finished.push_back(DatagramCompletion {
                    user_data,
                    result,
                    op: DatagramOp::Send { buffer, to },
                });
            }
        }
        Ok(())
    }

    /// Appends finished operations to `completions`, first waiting until at least `min` have
    /// finished, and says how many it appended. Fails with [`ErrorKind::InvalidInput`] when
    /// `min` is more than [`DatagramQueue::in_flight`]. Receives wait as long as no datagram
    /// comes.
    pub fn reap(&mut self, completions: &mut Vec<DatagramCompletion>, min: usize) -> Result<usize> {
        self.reap_timeout(completions, min, Duration::MAX)
    }

    /// Like [`DatagramQueue::reap`], but waits at most about `timeout`, counting the waits the
    /// socket was asked for, so in a simulation the wait is on the simulated clock; when it
    /// runs out it returns what it appended, which may be fewer than `min`.
    pub fn reap_timeout(
        &mut self,
        completions: &mut Vec<DatagramCompletion>,
        min: usize,
        timeout: Duration,
    ) -> Result<usize> {
        if min > self.in_flight() {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let mut appended = 0;
        let mut left = timeout;
        loop {
            self.take_waiting();
            appended += self.drain(completions);
            if appended >= min || left.is_zero() || self.receives.is_empty() {
                return Ok(appended);
            }
            let wait = left.min(LONGEST_SINGLE_WAIT);
            let started = Instant::now();
            let timed_out = self.take_batch(wait);
            let spent = if timed_out { wait } else { started.elapsed() };
            left = left.saturating_sub(spent);
        }
    }

    /// Gives up the receives tagged `user_data`: each completes through a later reap with
    /// [`ErrorKind::Canceled`] and its untouched buffer. A finished operation keeps its own
    /// result. Fails with [`ErrorKind::NotFound`] when nothing in flight has that tag.
    pub fn cancel(&mut self, user_data: u64) -> Result<()> {
        let mut found = self.finished.iter().any(|done| done.user_data == user_data);
        let mut kept = VecDeque::with_capacity(self.receives.len());
        for (tag, buffer) in self.receives.drain(..) {
            if tag == user_data {
                found = true;
                self.finished.push_back(DatagramCompletion {
                    user_data: tag,
                    result: Err(Error::new(ErrorKind::Canceled)),
                    op: DatagramOp::Receive { buffer },
                });
            } else {
                kept.push_back((tag, buffer));
            }
        }
        self.receives = kept;
        if found {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::NotFound))
        }
    }

    /// How many operations were submitted and not yet reaped.
    pub fn in_flight(&self) -> usize {
        self.receives.len() + self.finished.len()
    }

    /// The most operations that may be in flight at once.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn drain(&mut self, completions: &mut Vec<DatagramCompletion>) -> usize {
        let count = self.finished.len();
        completions.extend(self.finished.drain(..));
        count
    }

    fn take_waiting(&mut self) {
        while let Some((_, buffer)) = self.receives.front_mut() {
            match self.socket.try_receive_from(buffer) {
                Ok(Some((length, from))) => self.finish_front(Ok(DatagramOutput::Received { length, from })),
                Ok(None) => return,
                Err(error) => {
                    self.finish_front(Err(error));
                    return;
                }
            }
        }
    }

    fn take_batch(&mut self, wait: Duration) -> bool {
        let mut received = Vec::new();
        let outcome = {
            let mut buffers: Vec<&mut [u8]> = self
                .receives
                .iter_mut()
                .take(MOST_PER_BATCH)
                .map(|(_, buffer)| &mut buffer[..])
                .collect();
            self.socket.receive_batch(&mut buffers, &mut received, wait)
        };
        match outcome {
            Err(error) if error.kind() == ErrorKind::TimedOut => true,
            Err(error) => {
                self.finish_front(Err(error));
                false
            }
            Ok(_) => {
                for (length, from) in received {
                    self.finish_front(Ok(DatagramOutput::Received { length, from }));
                }
                false
            }
        }
    }

    fn finish_front(&mut self, result: Result<DatagramOutput>) {
        if let Some((user_data, buffer)) = self.receives.pop_front() {
            self.finished.push_back(DatagramCompletion {
                user_data,
                result,
                op: DatagramOp::Receive { buffer },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DatagramOp, DatagramOutput, DatagramQueue};
    use crate::net::{DatagramSocket, NetQueueOptions};
    use crate::{ErrorKind, OsDatagramSocket};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;

    fn loopback() -> Arc<OsDatagramSocket> {
        Arc::new(OsDatagramSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap())
    }

    fn receive(bytes: usize) -> DatagramOp {
        DatagramOp::Receive {
            buffer: vec![0; bytes].into(),
        }
    }

    #[test]
    fn a_burst_of_datagrams_completes_the_receives_in_the_order_they_were_submitted() {
        let socket = loopback();
        let sender = loopback();
        let mut queue = DatagramQueue::new(socket.clone(), NetQueueOptions::new()).unwrap();
        for tag in 0..3 {
            queue.submit(receive(32), tag).unwrap();
        }
        let to = socket.local_address().unwrap();
        for datagram in [&b"one"[..], b"two", b"three"] {
            sender.send_to(datagram, to).unwrap();
        }
        let mut completions = Vec::new();
        let reaped = queue
            .reap_timeout(&mut completions, 3, Duration::from_secs(10))
            .unwrap();
        assert_eq!(reaped, 3);
        let texts: Vec<(u64, Vec<u8>)> = completions
            .into_iter()
            .map(|completion| {
                let Ok(DatagramOutput::Received { length, from }) = completion.result else {
                    panic!("not received");
                };
                assert_eq!(from, sender.local_address().unwrap());
                (completion.user_data, completion.op.into_buffer()[..length].to_vec())
            })
            .collect();
        assert_eq!(
            texts,
            vec![(0, b"one".to_vec()), (1, b"two".to_vec()), (2, b"three".to_vec())]
        );
        assert_eq!(queue.in_flight(), 0);
    }

    #[test]
    fn sends_complete_at_the_next_reap_and_reach_the_peer() {
        let socket = loopback();
        let peer = loopback();
        let mut queue = DatagramQueue::new(socket, NetQueueOptions::new()).unwrap();
        let to = peer.local_address().unwrap();
        queue
            .submit(DatagramOp::Send { buffer: b"hello".to_vec().into(), to }, 7)
            .unwrap();
        let mut completions = Vec::new();
        assert_eq!(queue.reap(&mut completions, 1).unwrap(), 1);
        assert!(matches!(completions[0].result, Ok(DatagramOutput::Sent(5))));
        let mut buffer = [0; 16];
        let (length, _) = peer.receive_from(&mut buffer, Duration::from_secs(5)).unwrap();
        assert_eq!(&buffer[..length], b"hello");
    }

    #[test]
    fn a_timed_reap_with_nothing_arriving_returns_none_and_keeps_the_receive() {
        let mut queue = DatagramQueue::new(loopback(), NetQueueOptions::new()).unwrap();
        queue.submit(receive(8), 1).unwrap();
        let mut completions = Vec::new();
        let reaped = queue
            .reap_timeout(&mut completions, 1, Duration::from_millis(20))
            .unwrap();
        assert_eq!(reaped, 0);
        assert_eq!(queue.in_flight(), 1);
        queue.cancel(1).unwrap();
        assert_eq!(queue.reap(&mut completions, 1).unwrap(), 1);
        assert_eq!(completions[0].result.as_ref().unwrap_err().kind(), ErrorKind::Canceled);
        assert_eq!(queue.cancel(1).unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[test]
    fn a_full_queue_hands_the_operation_back_and_too_large_a_minimum_is_refused() {
        let mut queue =
            DatagramQueue::new(loopback(), NetQueueOptions::new().capacity(1)).unwrap();
        queue.submit(receive(8), 1).unwrap();
        let rejected = queue.submit(receive(8), 2).unwrap_err();
        assert_eq!(rejected.error.kind(), ErrorKind::QueueFull);
        let mut completions = Vec::new();
        assert_eq!(
            queue.reap(&mut completions, 2).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert!(DatagramQueue::new(loopback(), NetQueueOptions::new().capacity(0)).is_err());
    }
}
