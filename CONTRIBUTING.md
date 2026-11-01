# Contributing to EggDB

Thank you for helping. EggDB is one database for every kind of data, written from scratch in
Rust, and open source under MIT OR Apache-2.0. These rules keep it ours, safe and fast.

## Clean room (D1)
- Write everything yourself, in Rust, from public specifications, papers and protocol
  documentation. Never copy code from another database, and never read the source of a copyleft
  or source-available database (MySQL, MariaDB, MongoDB, Redis's recent versions, Elasticsearch
  and others) while working on EggDB.
- No third-party crates in what we ship. Tests, benchmarks and build tools may use a permissive
  (MIT, Apache-2.0, BSD) crate after it is approved; never GPL, LGPL, AGPL, MPL, SSPL or BSL.
- Other products' names are trademarks: say "compatible with".

## Sign your work (Developer Certificate of Origin)
Every commit carries a `Signed-off-by:` line (`git commit -s`), certifying the
[Developer Certificate of Origin 1.1](https://developercertificate.org/): you wrote the change,
or have the right to submit it under the project's licences. There is no contributor licence
agreement.

## How the code is written
- No panics on input: everything a client, a file or the network sends is parsed into errors as
  values, and parsers get fuzz targets (D9).
- No comments in code except rustdoc on public items and one `// SAFETY:` line per `unsafe`
  block, each block holding one unsafe operation. Names, small functions, types and tests carry
  the explanations; reasons go in the PR and in `docs/decisions/`.
- Platform code lives only in `crates/eggdb-platform` (D8). The engine never depends on a
  protocol or a platform.
- Every hot path gets a benchmark (D7). Sizes and start time may only shrink (D6).

## Before you open a pull request
```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```
Describe what users can now do and how it was tested. An architecture decision gets a new file in
`docs/decisions/` and a line in `docs/decisions.md`; the roadmap line moves with the work.
Security-critical changes (cryptography, authentication, the file format) also get a
security-focused review (D35).

## Governance
The owner decides, and every decision is public in `docs/decisions/`. Security reports go
privately through [SECURITY.md](SECURITY.md). Everyone follows the
[code of conduct](CODE_OF_CONDUCT.md).
