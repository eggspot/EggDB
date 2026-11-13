# Leaderless quorums over the network

`eggdb serve --quorum 1=a:7400,2=b:7400,3=c:7400 --quorum-node 1` makes a server one node of a
leaderless cluster (D225, D1097). Every node takes reads and writes for every key; a key's `n` home
nodes come from rendezvous hashing of the key with each node's id. A write succeeds once `w` of
them have it; a read reconciles what `r` of them hold. Concurrent writes meet by the cluster's
resolution: last writer wins, siblings, or a counter or set CRDT.

## From Rust

```rust,ignore
use eggdb::cluster::quorum::VersionVector;
use eggdb::cluster::quorum_net::QuorumClient;

let mut client = QuorumClient::connect("127.0.0.1:7400", std::time::Duration::from_secs(5))?;
client.login(b"the server's password")?;
let read = client.get(b"cart:7")?;
client.put(b"cart:7", b"eggs, milk", &read.context)?;
```

Write back with the context of what you read, so the new version supersedes it; a write with an
empty context is concurrent with everything already there.

## The frames

Each frame is a big-endian `u32` length (at most 64 MiB) and that many bytes: a kind byte, then
its fields. Byte strings are a big-endian `u32` length and the bytes; an optional byte string is
`0`, or `1` and the string; a version vector is a `u32` count of (`u64` node, `u64` count) pairs;
a version is a vector, a stamp (`u64` milliseconds, `u32` counter, `u64` node) and an optional value
(absent for a deletion). Every integer is big-endian.

| Kind | Request | Fields |
|------|---------|--------|
| 8 | Hello | the password; the first frame on every connection to a server |
| 1 | Put | key, optional value (absent deletes), the context vector |
| 2 | Get | key |
| 3 | Store | key, version (between nodes) |
| 4 | Fetch | key (between nodes) |
| 5 | Hint | `u64` owner, key, version (between nodes) |
| 6 | Tree | `u64` asking node (anti-entropy) |
| 7 | KeysAt | `u64` asking node, a `u32` count of `u32` leaves |

| Kind | Answer | Fields |
|------|--------|--------|
| 0x80 | Ack | none |
| 0x81 | Written | the version written |
| 0x82 | Values | a `u32` count of optional values (siblings, in stamp order), the context vector |
| 0x83 | Versions | a `u32` count of versions |
| 0x84 | Leaves | a `u32` count of `u128` leaf digests |
| 0x85 | Keys | a `u32` count of keys |
| 0x86 | Failed | the reason, as UTF-8 |

A node answers a damaged frame with Failed and a wrong password with Failed, then closes the
connection.
