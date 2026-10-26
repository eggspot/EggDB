# EggDB file format, version 1

This is the specification of EggDB's own file format (D20). The design behind it is D77. Every
release reads every file an earlier release wrote. From v0.2.0 on, a committed sample file
checks that in CI.

Status: the checksum, the superblock and the page header are implemented in
`crates/eggdb-storage` (`format.rs`). The other sections are specified here first and
implemented by the v0.2.0 items in [roadmap.md](roadmap.md). A section may still change until
v0.2.0 ships, because no file has been released before it. After that, changes follow
"Compatibility" below.

## Conventions
- Every integer is little-endian and unsigned unless stated otherwise.
- **Checksums** are CRC-32C (Castagnoli, the reflected polynomial `0x82F63B78`), as RFC 3720
  defines it. That means an initial value of `0xFFFFFFFF` and a final XOR of `0xFFFFFFFF`. The
  check value of `"123456789"` is `0xE3069283`. Processors with CRC instructions compute it
  in hardware: SSE 4.2 on x86-64, the CRC extension on AArch64.
- A **page number** is a `u64`. Its top 16 bits are reserved for a volume number. They are
  written as zero, and a reader refuses any other value. Page `n` starts at byte
  `n × page_size`.
- **Page 0 never names a data page**, because it always holds the first header slot. So `0`
  means "no page" (`NO_PAGE`) in every page pointer.

## File layout
| Bytes | Contents |
|-------|----------|
| 0 – 4095 | header slot 0 |
| 4096 – 8191 | header slot 1 |
| from page `first_data_page` | data pages, free-list pages and log segments, in any order |

- The two header slots always take the first 8 KiB, whatever the page size. With 4 KiB pages,
  pages 0 and 1 are the slots and data starts at page 2. With larger pages, page 0 holds both
  slots and data starts at page 1.
- The page size is a power of two from 4 KiB to 64 KiB (`page_size_log2` from 12 to 16). It is
  chosen at creation (default 4 KiB) and never changes.
- The write-ahead log lives **inside the same file**, as a chain of segments (see "Write-ahead
  log"). A database is one file, with no sidecar log or lock file (D3). One sync of that file
  makes both the log and the data durable.
- The file grows at its end, one page or one log segment at a time. Pages freed inside it go on
  the free list and are reused before it grows.

## Header slots and the superblock
Each slot holds a 512-byte superblock followed by zeros. Updating the header writes the slot
the current superblock is *not* in, with `generation` raised by one, and then syncs the file. A
superblock with an even generation belongs in slot 0, and one with an odd generation in
slot 1. A superblock found in the other slot is refused.

| Offset | Size | Field | Meaning |
|-------:|-----:|-------|---------|
| 0 | 8 | `magic` | `45 67 67 44 42 0D 1A 0A` ("EggDB\r\x1A\n"). The control bytes catch text-mode transfers, as PNG's signature does. |
| 8 | 4 | `checksum` | CRC-32C of bytes 0–7 followed by bytes 12–511 |
| 12 | 2 | `format_version` | 1. A reader refuses 0 and anything newer than it knows. |
| 14 | 1 | `page_size_log2` | 12 to 16 |
| 15 | 1 | `page_reserved_tail` | bytes reserved at the end of every page for later encryption tags. 0 in version 1, and a reader refuses anything else. |
| 16 | 8 | `generation` | raised by one on every header update |
| 24 | 8 | `features_compatible` | see "Feature flags" |
| 32 | 8 | `features_read_only_compatible` | see "Feature flags" |
| 40 | 8 | `features_incompatible` | see "Feature flags" |
| 48 | 16 | `file_id` | random at creation. It seeds every page checksum. |
| 64 | 8 | `page_count` | pages in the file, header pages included. At least `first_data_page`. |
| 72 | 8 | `free_list_head` | first free-list page, or 0 |
| 80 | 8 | `free_page_count` | pages on the free list, the free-list pages themselves included. 0 exactly when `free_list_head` is 0. |
| 88 | 8 | `catalog_root` | root page of the catalog tree, or 0 in an empty file |
| 96 | 8 | `checkpoint_lsn` | the log position recovery replays from |
| 104 | 8 | `log_head_page` | first page of the oldest live log segment, or 0 |
| 112 | 8 | `log_head_sequence` | sequence number of that segment |
| 120 | 8 | `clock_high_water` | the highest hybrid-clock stamp handed out before the last checkpoint |
| 128 | 4 | `log_segment_pages` | pages per log segment, at least 1 (default: 64 KiB of pages) |
| 132 | 4 | reserved | written as zero |
| 136 | 56 | `recycled_segments` | seven first pages of log segments emptied by checkpoints, kept for reuse before the file grows. 0 marks an unused entry. |
| 192 | 8 | reserved | written as zero |
| 200 | 32 | reserved for the consensus hard state (term and vote, M15) | written as zero |
| 232 | 280 | reserved | written as zero |

Every page pointer (`free_list_head`, `catalog_root`, `log_head_page` and the recycled
segments) must be 0 or lie between `first_data_page` and `page_count − 1`. Reserved fields are
written as zero and ignored on read, so a later version can use them behind a feature flag.

**Opening a file:**
1. Read both slots. A slot is *intact* when it has the magic and its checksum holds.
2. Take the intact slot with the higher generation. If neither is intact, the error is "not a
   database" when neither slot has the magic, and "no valid header" otherwise.
3. Check that slot fully: version, page size, slot parity and pointers. A slot that is intact
   but refused (a newer version, say) refuses the whole file. The reader never falls back to
   the older slot, because that would silently open an older state of a file a newer release
   wrote.
4. Check the feature flags against the access asked for.

A crash while a slot is being written leaves that slot torn and the other one intact, so the
file opens in the old state or the new one. The superblock fits in the first 512 bytes, one
sector, so once a write has reached that far the slot holds the whole new superblock.

## Feature flags
There are three 64-bit sets, with ext4's rules for a bit a release doesn't know:
- **compatible:** ignored.
- **read-only compatible:** the file may be opened for reading, never for writing.
- **incompatible:** the file is refused.

Version 1 defines no bits. Anything a later release adds that an older release would get wrong
sets a bit in the right set, so an older release never misreads a newer file. That includes new
page types, key schemes other than 0, compression and encryption.

## Pages
Every page except the header slots starts with a 16-byte header:

| Offset | Size | Field | Meaning |
|-------:|-----:|-------|---------|
| 0 | 4 | `checksum` | see below |
| 4 | 1 | `page_type` | see the table below. 0 is never valid, so a page that was never written is refused. |
| 5 | 1 | `layout` | the layout version of this page type |
| 6 | 2 | `flags` | reserved for per-page compression and encryption. 0 in version 1. |
| 8 | 8 | `lsn` | the log position of the last change to the page |

**The checksum** is CRC-32C, computed as
`crc32c_append(crc32c_append(crc32c(file_id), page_number as 8 bytes), page[4..page_size])`.
Mixing the file's identity and the page number into the seed means a page written to the wrong
place, a stale page from another file, and a torn or damaged page all fail the same check. The
page doesn't store its own number. Header-slot pages and page numbers with volume bits set are
refused before anything is checked.

| `page_type` | Page |
|------------:|------|
| 1 | free-list page: a count, then the numbers of free pages |
| 2 | B+tree inner page |
| 3 | B+tree leaf page |
| 4 | overflow page, holding part of a large value |
| 5 | first page of a log segment |
| 6 – 15 | reserved for version 1's later items |
| 16 – 255 | reserved for later structures, such as immutable sorted runs and columnar pages, each behind an incompatible feature flag |

The body of each page type is specified by the item that implements it.

## Write-ahead log (item 4 of the v0.2.0 plan)
- **Segments:** the log is a chain of segments inside the file. Each segment is
  `log_segment_pages` consecutive pages. Its first page has page type 5 and a segment header
  holding the segment's sequence number and the log position it starts at.
- **Growing and recycling:** a new segment comes from the recycled list or from the end of the
  file. The old segment ends with a link record naming the next segment's first page and the
  new `page_count`, so recovery learns about the growth by replaying the log. That avoids
  needing the free list, which itself depends on the log.
- **Records:** each record is framed by its length, type, flags and a CRC-32C. The checksum's
  seed mixes in `file_id` and the segment's sequence number, so bytes left over from a recycled
  segment never validate.
- **Positions:** a log sequence number (LSN) is a byte position in the logical log, and it only
  grows. Each record also has room for a consensus term and index, as varints that take zero
  bytes when standalone (M15), and the commit record carries the hybrid-clock stamp.
- **Flushes:** a flush pads to the end of a 4 KiB block, so a block that has been synced is
  never rewritten.
- **Group commit:** one leader syncs for every committer waiting in its group.
- **Torn tail or corruption:** an invalid record counts as a torn tail only if no valid record
  follows it in the segment. A damaged record with valid records after it is reported as
  corruption with its position, never truncated silently (Alagappan et al., FAST'18).
- **Truncation:** the head is truncated by moving `log_head_page` past segments a checkpoint
  made redundant and recycling them. The tail is truncated by starting a new segment with a
  higher sequence number.
- **Failed syncs:** a failed sync is never retried. The database stops accepting writes and
  recovers from the durable state (D59).

## Recovery (item 7)
Recovery is analysis and redo from `checkpoint_lsn`, with no undo pass:
- **No uncommitted page reaches the file.** In v0.2.0 a write transaction's changed pages stay
  in the page cache until its commit record is in the log. A transaction that would need more
  changed pages than the cache holds fails with a readable error. M3's versions in the leaves
  lift that limit.
- **A checkpoint** writes the changed pages, syncs, and then writes the next superblock.
- **Torn pages:** the first change to a page after a checkpoint logs the whole page, so a page
  torn by a crash is rebuilt from the log.

## Keys and trees (items 5 and 6)
- **The catalog tree** maps each tree's identity to its root page, its key scheme (a byte) and
  room for partition bounds.
- **Key scheme 0**, the only scheme in v0.2.0, is the raw key compared as unsigned bytes.
- **Tenant and shard prefixes (D20):** a later scheme encodes `(tenant, shard, key)` into the
  same byte string, preserving order, and sets an incompatible feature bit.
- **Prefix truncation:** B+tree pages keep low and high fence keys and store the prefix those
  keys share only once. So an unused prefix costs nothing, and a used one costs about nothing
  per entry.

## Hybrid-clock stamps (item 9)
- A commit stamp is a `u64`: 52 bits of microseconds since **2026-01-01T00:00:00Z** (Unix time
  1767225600), followed by a 12-bit logical counter.
- It runs out around the year 2168.
- Stamp 0 means "none".
- The stamp never goes backwards: each new stamp is above the last one handed out and above
  `clock_high_water`, whatever the wall clock does (Kulkarni et al., "Logical Physical Clocks").

## Durability modes (item 4)
| Mode | A commit returns after | A power loss can |
|------|------------------------|------------------|
| `Full` (default) | its log group is synced | lose nothing that was committed |
| `Normal` | its log records are written; syncs happen at checkpoints | lose the latest commits, never corrupt |
| `Off` | its records are written; nothing is ever synced | corrupt the file (for bulk loads and tests) |

On Apple's systems, a sync that must reach the disk uses `F_FULLFSYNC`.

## Locking (item 2)
- An open for writing takes an exclusive lock on the database file itself, through the platform
  layer:
  - open-file-description locks on Linux;
  - `flock` on Apple's systems and the BSDs;
  - `LockFileEx` on Windows, on a byte range past any data.
- A second open fails at once, even from the same process.
- A read-only open takes a shared lock.
- The lock disappears when the process ends, so there are no stale locks.

## Compatibility
- Every later release reads version-1 files.
- A change that an older release would misread needs a feature flag (see "Feature flags").
  Within a page type, `layout` versions the body.
- `format_version` rises only for a change that feature flags can't express.
- The v0.2.0 sample file, committed to the repository, is opened and checked by CI on every
  change from then on (D20, D35).
