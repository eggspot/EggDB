use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use eggdb_platform::{
    Address, NetBackend, NetListener, NetOp, NetQueue, NetQueueOptions, Network, OsNetQueue,
    OsNetwork, OsStream,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

pub(crate) const ROUND_TRIP: &str = "net.round_trip";

struct Connected {
    queue: OsNetQueue,
    client: Arc<OsStream>,
    server: Arc<OsStream>,
}

fn connected(backend: NetBackend) -> Result<Connected, String> {
    let network = OsNetwork::new();
    let failed = |error: eggdb_platform::Error| error.to_string();
    let listener = Arc::new(
        network
            .listen(Address::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))))
            .map_err(failed)?,
    );
    let mut queue = network
        .net_queue(NetQueueOptions::new().backend(backend))
        .map_err(failed)?;
    let accept = NetOp::Accept {
        listener: Arc::clone(&listener),
    };
    let connect = NetOp::Connect {
        address: listener.local_address().map_err(failed)?,
    };
    queue.submit(accept, 0).map_err(|rejected| rejected.to_string())?;
    queue.submit(connect, 1).map_err(|rejected| rejected.to_string())?;
    let mut completions = Vec::new();
    queue.reap(&mut completions, 2).map_err(failed)?;
    completions.sort_by_key(|completion| completion.user_data);
    let mut streams = completions.into_iter().map(|completion| {
        completion
            .result
            .map_err(failed)
            .and_then(|output| output.into_stream().ok_or_else(|| "no connection".to_owned()))
    });
    let server = Arc::new(streams.next().ok_or("no accepted connection")??);
    let client = Arc::new(streams.next().ok_or("no connected connection")??);
    Ok(Connected {
        queue,
        client,
        server,
    })
}

fn round_trips(connected: &mut Connected, iterations: u64) -> Result<(), String> {
    let mut completions = Vec::with_capacity(2);
    for _ in 0..iterations {
        let receive = NetOp::Receive {
            stream: Arc::clone(&connected.server),
            buffer: vec![0; 1].into(),
        };
        let send = NetOp::Send {
            stream: Arc::clone(&connected.client),
            buffer: vec![7; 1].into(),
        };
        connected
            .queue
            .submit(receive, 0)
            .map_err(|rejected| rejected.to_string())?;
        connected
            .queue
            .submit(send, 1)
            .map_err(|rejected| rejected.to_string())?;
        completions.clear();
        while completions.len() < 2 {
            connected
                .queue
                .reap(&mut completions, 1)
                .map_err(|error| error.to_string())?;
        }
        for completion in completions.drain(..) {
            completion.result.map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn round_trip(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let automatic_backend = OsNetwork::new()
        .net_queue(NetQueueOptions::new())
        .map_err(|error| error.to_string())?
        .backend();
    let completion_backend = [NetBackend::IoUring, NetBackend::Iocp]
        .into_iter()
        .find(|backend| connected(*backend).is_ok());
    let mut automatic = connected(automatic_backend)?;
    let Some(completion_backend) = completion_backend else {
        let mut second = connected(automatic_backend)?;
        let measured = measure::paired(
            &mut |iterations| round_trips(&mut automatic, iterations),
            &mut |iterations| round_trips(&mut second, iterations),
        )?;
        return Ok(vec![Metric::reported(
            ROUND_TRIP,
            "automatic_backend_ns",
            measured.subject_ns,
        )]);
    };
    let mut completion = connected(completion_backend)?;
    let measured = measure::paired(
        &mut |iterations| round_trips(&mut completion, iterations),
        &mut |iterations| round_trips(&mut automatic, iterations),
    )?;
    Ok(vec![
        Metric::ratio(
            ROUND_TRIP,
            "completion_backend_to_automatic_backend",
            measured.ratio,
        ),
        Metric::reported(ROUND_TRIP, "completion_backend_ns", measured.subject_ns),
        Metric::reported(ROUND_TRIP, "automatic_backend_ns", measured.reference_ns),
    ])
}
