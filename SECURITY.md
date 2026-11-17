# Security policy

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/eggspot/EggDB/security/advisories/new),
not in public issues or pull requests.

Include what you found, how to reproduce it, and the version or commit you tested. We aim to
acknowledge a report within three working days. We fix confirmed issues under coordinated
disclosure, and publish an advisory with a CVE once a fix is released.

## Scope

In scope, in every release and on `main`:

- **Denial of service from hostile input.** Anything a client, a file or the network sends that
  crashes, hangs or exhausts the server or the embedded library: a panic reached from input, a
  parser that loops or recurses without bound, memory or disk taken beyond the configured limits,
  or a statement that escapes its timeout. This covers every wire protocol and query language
  EggDB speaks, and every file format it reads (its own, SQLite's, dumps, Parquet, Arrow, BSON,
  images and the rest).
- **Sandbox escapes.** WebAssembly functions and extensions, stored procedures and triggers in
  every dialect, and the AI features: anything that reaches memory, files, the network or
  privileges its limits and grants do not give it, or that runs past its CPU, memory, time or
  output limits.
- **Protocol attacks.** Authentication bypasses and downgrades (SCRAM, TLS, Kerberos, LDAP,
  OAuth, SAML, API keys, one-time codes), privilege escalation through roles, grants, owners,
  row security or column privileges, request smuggling and injection across the protocols, and
  cryptographic weaknesses in encryption at rest, backups and key handling.
- **Data integrity.** A committed write lost or corrupted by a crash, a power cut or a hostile
  file, and a checksum or ledger check that misses tampering.

Out of scope: attacks that need the operating system's administrator, physical access or the
server's own credentials; the operator's choice to listen without TLS beyond loopback; load that
any server of its size would refuse, sent within the limits the operator configured; and third-
party drivers and tools.

## Supported versions

Until 1.0, only the latest release and `main` receive security fixes. Each release's archives
carry SHA-256 checksums and a signed build provenance attestation
(`gh attestation verify <archive> -R eggspot/EggDB`).
