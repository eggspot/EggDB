use super::{SimHost, SimNetwork, World, is_cut, lock};
use crate::error::{Error, ErrorKind, Result};
use crate::net::{DatagramSocket, MAX_DATAGRAM_BYTES};
use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

#[derive(Debug, Default)]
pub(super) struct Inbox {
    bytes: usize,
    datagrams: BTreeMap<(Duration, u64), (SocketAddr, Vec<u8>)>,
}

impl Inbox {
    fn take_due(&mut self, deadline: Duration) -> Option<(Duration, SocketAddr, Vec<u8>)> {
        let key = *self.datagrams.first_key_value()?.0;
        if key.0 > deadline {
            return None;
        }
        let (from, bytes) = self.datagrams.remove(&key)?;
        self.bytes = self.bytes.saturating_sub(bytes.len());
        Some((key.0, from, bytes))
    }
}

/// A UDP socket of a [`SimHost`]. Dropping it unbinds its port and loses what it has not
/// received.
pub struct SimDatagramSocket {
    network: SimNetwork,
    address: SocketAddr,
}

impl fmt::Debug for SimDatagramSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimDatagramSocket")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

fn free_port(world: &World, ip: IpAddr) -> Option<SocketAddr> {
    SimNetwork::EPHEMERAL_PORTS
        .map(|port| SocketAddr::new(ip, port))
        .find(|address| !world.datagram_sockets.contains_key(address))
}

pub(super) fn bind(host: &SimHost, address: SocketAddr) -> Result<SimDatagramSocket> {
    let mut world = lock(&host.network.shared);
    if !address.ip().is_unspecified() && address.ip() != host.ip {
        return Err(Error::new(ErrorKind::AddrNotAvailable));
    }
    let address = if address.port() == 0 {
        free_port(&world, host.ip).ok_or_else(|| Error::new(ErrorKind::AddrNotAvailable))?
    } else {
        SocketAddr::new(host.ip, address.port())
    };
    if world.datagram_sockets.contains_key(&address) {
        return Err(Error::new(ErrorKind::AddrInUse));
    }
    world.next_operation()?;
    world.datagram_sockets.insert(address, Inbox::default());
    Ok(SimDatagramSocket {
        network: host.network.clone(),
        address,
    })
}

impl DatagramSocket for SimDatagramSocket {
    fn local_address(&self) -> Result<SocketAddr> {
        Ok(self.address)
    }

    fn send_to(&self, datagram: &[u8], to: SocketAddr) -> Result<usize> {
        if datagram.len() > MAX_DATAGRAM_BYTES {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let mut world = lock(&self.network.shared);
        world.next_operation()?;
        let from = self.address;
        if is_cut(&world.cuts, from.ip(), to.ip()) {
            return Ok(datagram.len());
        }
        if world.packet_lost(from.ip(), to.ip()) {
            return Ok(datagram.len());
        }
        let copies = if world.packet_duplicated(from.ip(), to.ip()) {
            2
        } else {
            1
        };
        let now = world.now();
        for _ in 0..copies {
            let crossed = world.arrival(now, from.ip(), to.ip(), datagram.len());
            let arrival = crossed.saturating_add(world.held_back(from.ip(), to.ip()));
            let sequence = world.next_datagram;
            world.next_datagram = sequence.wrapping_add(1);
            if let Some(inbox) = world.datagram_sockets.get_mut(&to) {
                let bytes = inbox.bytes.saturating_add(datagram.len());
                if bytes <= SimNetwork::DATAGRAM_BUFFER_BYTES {
                    inbox.bytes = bytes;
                    inbox
                        .datagrams
                        .insert((arrival, sequence), (from, datagram.to_vec()));
                }
            }
        }
        Ok(datagram.len())
    }

    fn receive_from(&self, buffer: &mut [u8], wait: Duration) -> Result<(usize, SocketAddr)> {
        if buffer.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let mut world = lock(&self.network.shared);
        world.next_operation()?;
        let deadline = world.now().saturating_add(wait);
        let due = world
            .datagram_sockets
            .get_mut(&self.address)
            .and_then(|inbox| inbox.take_due(deadline));
        match due {
            Some((arrival, from, bytes)) => {
                world.advance_to(arrival);
                let length = bytes.len().min(buffer.len());
                for (into, byte) in buffer.iter_mut().zip(&bytes) {
                    *into = *byte;
                }
                Ok((length, from))
            }
            None => {
                world.advance_to(deadline);
                Err(Error::new(ErrorKind::TimedOut))
            }
        }
    }
}

impl Drop for SimDatagramSocket {
    fn drop(&mut self) {
        lock(&self.network.shared)
            .datagram_sockets
            .remove(&self.address);
    }
}

#[cfg(test)]
mod tests {
    use crate::clock::Clock;
    use crate::net::{DatagramSocket, MAX_DATAGRAM_BYTES, Network};
    use crate::{ErrorKind, SimNetwork};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    const A: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    const B: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

    fn any_port(ip: IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 0)
    }

    #[test]
    fn a_datagram_arrives_whole_after_the_latency_and_moves_the_clock_to_it() {
        let network = SimNetwork::new();
        network.latency(Duration::from_millis(5), Duration::from_millis(5));
        let sender = network.host(A).bind_datagram(any_port(A)).unwrap();
        let receiver = network.host(B).bind_datagram(any_port(B)).unwrap();
        let to = receiver.local_address().unwrap();
        assert_eq!(sender.send_to(b"<34>1 - - - - - - hi", to).unwrap(), 20);
        let mut buffer = [0; 64];
        let (length, from) = receiver
            .receive_from(&mut buffer, Duration::from_secs(1))
            .unwrap();
        assert_eq!(&buffer[..length], b"<34>1 - - - - - - hi");
        assert_eq!(from, sender.local_address().unwrap());
        assert_eq!(network.clock().monotonic(), Duration::from_millis(5));
    }

    #[test]
    fn lost_datagrams_never_arrive_and_duplicated_ones_arrive_twice() {
        let network = SimNetwork::new();
        let sender = network.host(A).bind_datagram(any_port(A)).unwrap();
        let receiver = network.host(B).bind_datagram(any_port(B)).unwrap();
        let to = receiver.local_address().unwrap();
        network.packet_loss(1_000_000);
        sender.send_to(b"gone", to).unwrap();
        let error = receiver
            .receive_from(&mut [0; 8], Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        network.packet_loss(0);
        network.packet_duplication(1_000_000);
        sender.send_to(b"twice", to).unwrap();
        for _ in 0..2 {
            let mut buffer = [0; 8];
            let (length, _) = receiver
                .receive_from(&mut buffer, Duration::from_secs(1))
                .unwrap();
            assert_eq!(&buffer[..length], b"twice");
        }
        let error = receiver
            .receive_from(&mut [0; 8], Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
    }

    #[test]
    fn a_held_back_datagram_is_overtaken_by_a_later_one() {
        let network = SimNetwork::new();
        let sender = network.host(A).bind_datagram(any_port(A)).unwrap();
        let receiver = network.host(B).bind_datagram(any_port(B)).unwrap();
        let to = receiver.local_address().unwrap();
        network.packet_reordering(1_000_000, Duration::from_secs(1));
        let mut held = false;
        for _ in 0..16 {
            sender.send_to(b"first", to).unwrap();
            network.packet_reordering(0, Duration::ZERO);
            sender.send_to(b"second", to).unwrap();
            let mut order = Vec::new();
            for _ in 0..2 {
                let mut buffer = [0; 8];
                let (length, _) = receiver
                    .receive_from(&mut buffer, Duration::from_secs(2))
                    .unwrap();
                order.push(buffer[..length].to_vec());
            }
            held |= order[0] == b"second";
            network.packet_reordering(1_000_000, Duration::from_secs(1));
        }
        assert!(held, "no held-back datagram was overtaken in 16 tries");
    }

    #[test]
    fn a_receive_with_nothing_due_in_time_times_out_at_the_end_of_its_wait() {
        let network = SimNetwork::new();
        network.latency(Duration::from_secs(2), Duration::from_secs(2));
        let sender = network.host(A).bind_datagram(any_port(A)).unwrap();
        let receiver = network.host(B).bind_datagram(any_port(B)).unwrap();
        sender
            .send_to(b"late", receiver.local_address().unwrap())
            .unwrap();
        let error = receiver
            .receive_from(&mut [0; 8], Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert_eq!(network.clock().monotonic(), Duration::from_secs(1));
        let (length, _) = receiver
            .receive_from(&mut [0; 8], Duration::from_secs(5))
            .unwrap();
        assert_eq!(length, 4);
        assert_eq!(network.clock().monotonic(), Duration::from_secs(2));
    }

    #[test]
    fn a_datagram_sent_across_a_cut_is_lost_even_after_the_cut_heals() {
        let network = SimNetwork::new();
        let a = network.host(A).bind_datagram(any_port(A)).unwrap();
        let b = network.host(B).bind_datagram(any_port(B)).unwrap();
        network.partition_one_way(A, B);
        a.send_to(b"lost", b.local_address().unwrap()).unwrap();
        b.send_to(b"back", a.local_address().unwrap()).unwrap();
        network.heal(A, B);
        let wait = Duration::from_millis(10);
        assert_eq!(
            b.receive_from(&mut [0; 8], wait).unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        assert_eq!(a.receive_from(&mut [0; 8], wait).unwrap().0, 4);
    }

    #[test]
    fn a_long_datagram_is_cut_to_the_buffer_and_an_oversized_one_is_refused() {
        let network = SimNetwork::new();
        let a = network.host(A).bind_datagram(any_port(A)).unwrap();
        let b = network.host(B).bind_datagram(any_port(B)).unwrap();
        let to = b.local_address().unwrap();
        a.send_to(b"0123456789", to).unwrap();
        let mut buffer = [0; 4];
        let (length, _) = b.receive_from(&mut buffer, Duration::ZERO).unwrap();
        assert_eq!((length, &buffer), (4, b"0123"));
        let oversized = vec![0; MAX_DATAGRAM_BYTES + 1];
        assert_eq!(
            a.send_to(&oversized, to).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            b.receive_from(&mut [], Duration::ZERO).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_full_receive_buffer_drops_what_arrives_beyond_it() {
        let network = SimNetwork::new();
        let a = network.host(A).bind_datagram(any_port(A)).unwrap();
        let b = network.host(B).bind_datagram(any_port(B)).unwrap();
        let to = b.local_address().unwrap();
        let datagram = vec![7; 60_000];
        for _ in 0..5 {
            a.send_to(&datagram, to).unwrap();
        }
        let mut buffer = vec![0; MAX_DATAGRAM_BYTES];
        let mut received = 0;
        while b.receive_from(&mut buffer, Duration::ZERO).is_ok() {
            received += 1;
        }
        assert_eq!(received, SimNetwork::DATAGRAM_BUFFER_BYTES / datagram.len());
    }

    #[test]
    fn ports_are_taken_until_the_socket_is_dropped_and_apart_from_tcp() {
        let network = SimNetwork::new();
        let host = network.host(A);
        let socket = host.bind_datagram(any_port(A)).unwrap();
        let address = socket.local_address().unwrap();
        assert_eq!(
            host.bind_datagram(address).unwrap_err().kind(),
            ErrorKind::AddrInUse
        );
        assert!(host.listen(crate::Address::Tcp(address)).is_ok());
        assert_eq!(
            host.bind_datagram(any_port(B)).unwrap_err().kind(),
            ErrorKind::AddrNotAvailable
        );
        drop(socket);
        assert!(host.bind_datagram(address).is_ok());
    }

    #[test]
    fn injected_faults_fail_one_datagram_operation_and_change_nothing() {
        let network = SimNetwork::new();
        let a = network.host(A).bind_datagram(any_port(A)).unwrap();
        let b = network.host(B).bind_datagram(any_port(B)).unwrap();
        let to = b.local_address().unwrap();
        network.fail_operation(network.operation_count());
        assert_eq!(
            a.send_to(b"x", to).unwrap_err().kind(),
            ErrorKind::InjectedFault
        );
        a.send_to(b"y", to).unwrap();
        network.fail_operation(network.operation_count());
        assert_eq!(
            b.receive_from(&mut [0; 1], Duration::ZERO)
                .unwrap_err()
                .kind(),
            ErrorKind::InjectedFault
        );
        let mut buffer = [0; 1];
        b.receive_from(&mut buffer, Duration::ZERO).unwrap();
        assert_eq!(&buffer, b"y");
    }
}
