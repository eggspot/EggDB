# Stability promise

EggDB versions follow semantic versioning (`MAJOR.MINOR.PATCH`, D21). Until 1.0, a minor release
may still change what is below when a decision says so; from 1.0 the promise is binding.

## What is promised
- **The C API and the language drivers** (`include/eggdb.h`, `drivers/`): a function, type or
  constant is never removed or changed incompatibly in a minor or patch release.
- **The wire protocols and SQL**: what a client sends that works keeps working; new features
  are added, and behaviour is fixed only toward the compatibility target it already names.
- **The file format** (D20, `docs/format.md`): every release opens the files of every earlier
  release, upgrading them online where needed, and the sample files of each release stay
  readable forever (D96). A file is never changed so that an older release would misread it.
  It refuses the file instead.
- **Configuration**: command-line options, environment variables and configuration keys keep
  their meaning.

## How things change
Deprecations are announced in the release notes one minor release ahead, with the replacement.
Removals happen only in a major release. After 1.0 there are release channels (stable, beta,
nightly) and a long-term-support line.

## What EggDB never does
EggDB sends nothing anywhere unless you turn it on: no telemetry, no update checks.

## Supply chain
Every release archive has a SHA-256 checksum and a signed build provenance attestation
(`gh attestation verify <archive> -R eggspot/EggDB`). CI checks that two builds of the same commit
are byte for byte identical, so anyone can rebuild a release and compare.
