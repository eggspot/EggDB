//! The behavior every `Network` shares, checked against the operating system's network on the
//! loopback interface and against the simulation on several seeds, with and without short
//! transfers.

use eggdb_platform::{
    Address, ErrorKind, NetCompletion, NetListener, NetOp, NetQueue, NetQueueOptions, NetStream,
    Network, OsNetwork, SimNetwork,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

macro_rules! contract {
    ($name:ident, |$net:ident| $body:block) => {
        #[test]
        fn $name() {
            fn run<N: Network>($net: &N) $body
            run(&OsNetwork::new());
            for seed in 0..8 {
                let network = SimNetwork::new();
                network.seed(seed);
                network.short_transfers(seed % 2 == 1);
                run(&network.host(LOOPBACK));
            }
        }
    };
}

type Completion<N> = NetCompletion<<N as Network>::Listener, <N as Network>::Stream>;

fn any_port() -> Address {
    Address::Tcp(SocketAddr::new(LOOPBACK, 0))
}

fn submit<Q: NetQueue>(queue: &mut Q, op: NetOp<Q::Listener, Q::Stream>, user_data: u64) {
    if let Err(rejected) = queue.submit(op, user_data) {
        panic!("submit failed: {rejected}");
    }
}

fn reap<N: Network>(queue: &mut N::Queue, min: usize) -> Vec<Completion<N>> {
    let mut completions = Vec::new();
    queue.reap(&mut completions, min).unwrap();
    completions
}

fn connect<N: Network>(
    queue: &mut N::Queue,
    listener: &Arc<N::Listener>,
) -> (Arc<N::Stream>, Arc<N::Stream>) {
    submit(
        queue,
        NetOp::Accept {
            listener: Arc::clone(listener),
        },
        1,
    );
    submit(
        queue,
        NetOp::Connect {
            address: listener.local_address().unwrap(),
        },
        2,
    );
    let mut completions = reap::<N>(queue, 2);
    completions.sort_by_key(|completion| completion.user_data);
    let mut streams = completions
        .into_iter()
        .map(|completion| Arc::new(completion.result.unwrap().into_stream().unwrap()));
    let server = streams.next().unwrap();
    let client = streams.next().unwrap();
    (client, server)
}

fn transfer<N: Network>(
    queue: &mut N::Queue,
    from: &Arc<N::Stream>,
    to: &Arc<N::Stream>,
    bytes: &[u8],
) -> Vec<u8> {
    const SEND: u64 = 10;
    const RECEIVE: u64 = 11;
    let mut sent = 0;
    let mut received = Vec::new();
    let mut sending = false;
    let mut receiving = false;
    while received.len() < bytes.len() {
        if !sending && sent < bytes.len() {
            let op = NetOp::Send {
                stream: Arc::clone(from),
                buffer: bytes[sent..].to_vec().into(),
            };
            submit(queue, op, SEND);
            sending = true;
        }
        if !receiving {
            let op = NetOp::Receive {
                stream: Arc::clone(to),
                buffer: vec![0; 4096].into(),
            };
            submit(queue, op, RECEIVE);
            receiving = true;
        }
        for completion in reap::<N>(queue, 1) {
            let count = completion.result.unwrap().transferred().unwrap();
            if completion.user_data == SEND {
                sent += count;
                sending = false;
            } else {
                assert!(count > 0, "the stream ended early");
                received.extend_from_slice(&completion.op.into_buffer().unwrap()[..count]);
                receiving = false;
            }
        }
    }
    if sending || receiving {
        let pending = queue.in_flight();
        assert_eq!(pending, usize::from(sending));
        reap::<N>(queue, pending);
    }
    received
}

contract!(bytes_cross_in_order_both_ways, |net| {
    let listener = Arc::new(net.listen(any_port()).unwrap());
    let mut queue = net.net_queue(NetQueueOptions::new()).unwrap();
    let (client, server) = connect::<N>(&mut queue, &listener);
    let request: Vec<u8> = (0..200_000_u32).map(|at| (at % 251) as u8).collect();
    assert_eq!(
        transfer::<N>(&mut queue, &client, &server, &request),
        request
    );
    let answer = b"ok".to_vec();
    assert_eq!(transfer::<N>(&mut queue, &server, &client, &answer), answer);
});

contract!(each_end_knows_both_addresses, |net| {
    let listener = Arc::new(net.listen(any_port()).unwrap());
    let mut queue = net.net_queue(NetQueueOptions::new()).unwrap();
    let (client, server) = connect::<N>(&mut queue, &listener);
    assert_eq!(
        client.peer_address().unwrap(),
        listener.local_address().unwrap()
    );
    assert_eq!(
        server.peer_address().unwrap(),
        client.local_address().unwrap()
    );
    assert_eq!(
        server.local_address().unwrap(),
        client.peer_address().unwrap()
    );
});

contract!(connecting_where_nothing_listens_is_refused, |net| {
    let address = net.listen(any_port()).unwrap().local_address().unwrap();
    let mut queue = net.net_queue(NetQueueOptions::new()).unwrap();
    submit(&mut queue, NetOp::Connect { address }, 0);
    let completion = reap::<N>(&mut queue, 1).pop().unwrap();
    assert_eq!(
        completion.result.unwrap_err().kind(),
        ErrorKind::ConnectionRefused
    );
});

contract!(listening_twice_at_one_address_is_refused, |net| {
    let listener = net.listen(any_port()).unwrap();
    let error = net.listen(listener.local_address().unwrap()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AddrInUse);
});

contract!(a_shut_down_end_delivers_its_bytes_then_ends, |net| {
    let listener = Arc::new(net.listen(any_port()).unwrap());
    let mut queue = net.net_queue(NetQueueOptions::new()).unwrap();
    let (client, server) = connect::<N>(&mut queue, &listener);
    assert_eq!(transfer::<N>(&mut queue, &client, &server, b"bye"), b"bye");
    client.shutdown_write().unwrap();
    let op = NetOp::Receive {
        stream: Arc::clone(&server),
        buffer: vec![0; 8].into(),
    };
    submit(&mut queue, op, 0);
    let completion = reap::<N>(&mut queue, 1).pop().unwrap();
    assert_eq!(completion.result.unwrap().transferred(), Some(0));
    assert_eq!(transfer::<N>(&mut queue, &server, &client, b"ack"), b"ack");
});

contract!(
    empty_receives_are_refused_and_empty_sends_do_nothing,
    |net| {
        let listener = Arc::new(net.listen(any_port()).unwrap());
        let mut queue = net.net_queue(NetQueueOptions::new()).unwrap();
        let (client, _server) = connect::<N>(&mut queue, &listener);
        let receive = NetOp::Receive {
            stream: Arc::clone(&client),
            buffer: Vec::new().into(),
        };
        let send = NetOp::Send {
            stream: Arc::clone(&client),
            buffer: Vec::new().into(),
        };
        submit(&mut queue, receive, 0);
        submit(&mut queue, send, 1);
        let mut completions = reap::<N>(&mut queue, 2);
        completions.sort_by_key(|completion| completion.user_data);
        let mut completions = completions.into_iter();
        let refused = completions.next().unwrap().result.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidInput);
        let sent = completions.next().unwrap().result.unwrap();
        assert_eq!(sent.transferred(), Some(0));
    }
);

contract!(a_full_queue_hands_the_operation_back, |net| {
    let listener = Arc::new(net.listen(any_port()).unwrap());
    let mut queue = net.net_queue(NetQueueOptions::new().capacity(1)).unwrap();
    assert_eq!(queue.capacity(), 1);
    let accept = || NetOp::Accept {
        listener: Arc::clone(&listener),
    };
    submit(&mut queue, accept(), 0);
    let rejected = queue.submit(accept(), 1).unwrap_err();
    assert_eq!(rejected.error.kind(), ErrorKind::QueueFull);
    assert!(matches!(rejected.op, NetOp::Accept { .. }));
    assert_eq!(queue.in_flight(), 1);
});

contract!(waiting_for_more_than_is_in_flight_is_refused, |net| {
    let mut queue = net.net_queue(NetQueueOptions::new()).unwrap();
    let mut completions = Vec::new();
    let error = queue.reap(&mut completions, 1).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(queue.reap(&mut completions, 0).unwrap(), 0);
});

contract!(queues_without_capacity_are_refused, |net| {
    let error = net
        .net_queue(NetQueueOptions::new().capacity(0))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
});
