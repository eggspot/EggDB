# EggDB file format, version 1

This is the specification of EggDB's own file format (D20). The design behind it is D77. Every
release reads every file an earlier release wrote. From v0.2.0 on, a committed sample file
checks that in CI.

Status: the checksum, the superblock, the page header, the free list, the write-ahead log and
the B+tree pages are implemented in `crates/eggdb-storage`. The other sections are specified here first and
implemented by the v0.2.0 items in [roadmap.md](roadmap.md). A section may still change until
v0.2.0 ships, because no file has been released before it. After that, changes follow
"Compatibility" below.

## Conventions
- Every integer is little-endian and unsigned unless stated otherwise.
- **Checksums detect accidental damage, not tampering.** A file crafted on purpose can pass
  them. Such a file must still only ever produce errors, never a crash or a hang (D9). Tamper
  detection comes later, with encryption at rest (D23), behind a feature flag.
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
| 128 | 4 | `log_segment_pages` | pages per log segment: at least 1, at most 64 MiB of pages, and at least one page plus 8 KiB once the file has a log (default: 64 KiB of pages, at least two) |
| 132 | 4 | reserved | written as zero |
| 136 | 56 | `recycled_segments` | seven first pages of log segments emptied by checkpoints, kept for reuse before the file grows. 0 marks an unused entry. |
| 192 | 8 | reserved | written as zero |
| 200 | 32 | reserved for the consensus hard state (term and vote, M15) | written as zero |
| 232 | 280 | reserved | written as zero |

Every page pointer (`free_list_head`, `catalog_root`, `log_head_page` and the recycled
segments) must be 0 or lie between `first_data_page` and `page_count − 1`. A log segment
starting at `log_head_page` or at a recycled entry must also end within `page_count`.
`page_count` must be below 2^48 (no volume bits). Opening a file also checks it against the
file's real length. Reserved fields are
written as zero and ignored on read, so a later version can use them behind a feature flag.

**Opening a file:**
1. Read both slots. A slot is *intact* when it has the magic and its checksum holds.
2. Take the intact slot with the higher generation. Two intact slots with the same generation
   refuse the file. If neither is intact, the error is "not a database" when neither slot has
   the magic, and "no valid header" otherwise.
3. Check that slot fully: version, page size, slot parity and pointers. A slot that is intact
   but refused (a newer version, or a superblock in the wrong slot) refuses the whole file.
   The reader never falls back to the older slot, because that would silently open an older
   state of a file a newer release wrote.
4. Check the feature flags against the access asked for.
5. If the other slot has the magic but fails its checksum, report it. A crash during a header
   update looks like that, and so does damage to a synced slot. In the second case the chosen
   superblock is older than the file. Recovery catches that case: it refuses the file when the
   segment at `log_head_page` doesn't carry `log_head_sequence`.

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
| 6 | 2 | `flags` | reserved for per-page compression and encryption. 0 in version 1, and a reader refuses anything else. |
| 8 | 8 | `lsn` | the log position of the last change to the page |

**The checksum** is CRC-32C, computed as
`crc32c_append(crc32c_append(crc32c(file_id), page_number as 8 bytes), page[4..page_size])`.
Mixing the file's identity and the page number into the seed means a page written to the wrong
place, a page from another database, and a torn or damaged page all fail the same check. The
page doesn't store its own number. After the buffer's length, header-slot pages and page
numbers with volume bits set are refused before the checksum is computed.

Two kinds of damage the checksum can't see:
- a page from a copy of the same file (`cp`, a restored backup), which shares its `file_id`;
- a lost write that left an older, intact version of the same page.

The page's `lsn`, checked against the log during recovery, is what catches those.

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

### Free-list pages (type 1, layout 1)
| Offset | Size | Field |
|-------:|-----:|-------|
| 16 | 8 | `next`: the next free-list page, or 0 |
| 24 | 4 | `count`: the entries on this page, at most `(page_size − 32) / 8` |
| 28 | 4 | reserved, zero |
| 32 | 8 × `count` | the free pages, as page numbers |

**Validity.** Starting at `free_list_head` and following `next`, the list pages plus their
entries number exactly `free_page_count`. Every number is a data page of the file and appears
once. None is the catalog root or a page of a live or recycled log segment. A list that breaks
any of these rules refuses the open. That includes a list that loops, points outside the file
or disagrees with the count. A crafted list naming other pages in use can't be told apart from
a real one here. `eggdb check` finds it, by checking that free, reachable and log pages are
disjoint and together account for every page.

**Commits.** A commit never overwrites the committed list:
1. The new list is written to pages that are free in the committed state, or to new pages at
   the end of the file.
2. The old list pages join the new list as free pages.
3. A page freed since the last commit is handed out again only after the commit that frees it.

So a crash before the new superblock is durable finds the old state whole (D78).

## Write-ahead log (item 4 of the v0.2.0 plan)
The log is a chain of segments inside the file (D77, D81). A commit appends a whole image of
every page it writes and a commit record, makes the log durable as its durability mode says,
and only then writes the pages in place. Opening the file replays the committed records since
`checkpoint_lsn`, so a page torn while being overwritten in place is rebuilt from the log.

**Creating the log.** A file is created without a log, so a file that is never changed stays
8 KiB. The first change appends a segment at the end of the file, writes its header, syncs,
and then writes the next superblock with `log_head_page`, `log_head_sequence` (the old value
plus one), `checkpoint_lsn` (the segment's start) and `log_segment_pages`, and syncs again. A
file with no log, such as one written before the log existed, opens as it is and gets its log
the same way. Until checkpoints (item 7), the header is written only then.

**Segments.** A segment is `log_segment_pages` consecutive pages. A file with a log needs
segments of at least one page plus 8 KiB, so the largest record fits in one; the default is
64 KiB of pages and at least two pages. A file without a log may hold a smaller value, which is
replaced when the log is created. The first 4 KiB block of a segment is its header:

| Offset | Size | Field | Meaning |
|-------:|-----:|-------|---------|
| 0 | 4 | `checksum` | CRC-32C seeded like a page's (`file_id`, then the page number), over bytes 4–4095 only |
| 4 | 1 | `page_type` | 5 |
| 5 | 1 | `layout` | 1 |
| 6 | 2 | `flags` | 0 |
| 8 | 8 | `start_lsn` | the LSN of the segment's byte 4096 |
| 16 | 8 | `sequence` | one more than the previous segment's |
| 24 | 4072 | reserved | zero |

Records follow from byte 4096. The checksum covers only the header block, because records are
appended to the rest of the segment after it is written.

**Positions.** An LSN is a byte position in the logical log: byte `o` (from 4096) of a segment
is LSN `start_lsn + o − 4096`, and the next segment starts at `start_lsn + segment bytes −
4096`. A page's header `lsn` is the LSN of the record that logged it.

**Records.** Each record is a 10-byte frame and a payload:

| Offset | Size | Field |
|-------:|-----:|-------|
| 0 | 4 | CRC-32C of bytes 4 to the end of the payload, seeded with `crc32c_append(crc32c_append(crc32c(file_id), sequence), lsn)`: the segment's sequence and then the record's own LSN, 8 bytes each |
| 4 | 4 | payload length |
| 8 | 1 | type |
| 9 | 1 | flags: 0. Bit 0 is reserved for a consensus term and index as varints after the frame (M15), so they take no bytes when standalone. A reader refuses any other value. |

| Type | Record | Payload |
|-----:|--------|---------|
| 1 | batch | `durable_lsn` (8): the LSN up to which the log was known to be synced when this flush was written |
| 2 | pad | zeros up to the end of the 4 KiB block |
| 3 | page image | the page number (8) and the whole page as it is written in place, its own checksum included |
| 4 | commit | the commit stamp (8, 0 until item 9), then `page_count`, `free_list_head`, `free_page_count`, `catalog_root`, `clock_high_water` and the three feature sets (8 each) |
| 5 | link | the next segment's first page (8) and the page count once it exists (8) |

- A record never spans segments, and never starts in the last 9 bytes of a 4 KiB block; those
  bytes are zero and skipped. A pad never crosses the end of its block.
- Mixing the LSN into the seed means a record copied or misdirected to another position, even
  within its segment, fails its check.
- A reader checks the type, the flags and the payload length the type requires (batch 8, page
  image 8 plus a page, commit 72, link 16, pad at most the rest of its block) before computing
  the checksum, so no crafted length makes it checksum more than one record's bytes.
- **Flushes:** each commit is one flush. It starts at a block boundary with a batch record, and
  ends with a pad to the end of its block, so a block that has been synced is never rewritten.
  The one exception is the first flush after opening a log whose last commit was not followed
  by its pad (see "Resuming"): that flush starts with a link record where the commit ended.
  The images of a transaction come before its commit record, and images after the last commit
  record belong to no commit.
- **Growing:** when a record would leave less room than a link record before the segment's
  end, a link record names the next segment, which comes from the end of the file, and the
  flush continues there after a batch record. A flush whose pad would reach the segment's end
  ends with a link instead. Recovery learns the file's growth from link records; it needs no
  free list. Reusing recycled segments, and moving `log_head_page`, are checkpoints' work
  (item 7).
- **Torn tail or corruption:** the log ends at the first record that fails its checks. That is
  a torn tail, and the file opens at the last commit before it, unless a valid batch record at
  a later block boundary of the same segment carries a `durable_lsn` above the bad record's
  LSN. Then the bad record had been synced, and the open fails with "the log is damaged" and
  the segment and offset (Alagappan et al., FAST'18). A segment named by a link whose header
  is wrong ends the log the same way. These are damage too, and refuse the open the same way:
  - a page image whose own page checksum fails, or that names a header slot, a page outside
    the file or a page of the log;
  - a link that overlaps an earlier segment, or whose page count isn't exactly the end of the
    segment it names;
  - a commit record whose fields the superblock would refuse, whose page count is below the
    header's, or whose page count is more than the file's real length in pages plus the number
    of committed page images. Every page a commit adds is logged as an image, so a valid log
    can't claim more; this stops a crafted record from growing the file.
- **Features:** the feature flags of the last commit record are checked against the access
  asked for before anything is written, so a release never changes a file it would refuse.
- **Resuming:** after replaying, appends continue at the block after the last commit. If any
  byte after that point in its segment is not zero (records of a flush that never completed),
  the next flush starts with a link to a fresh segment instead, so those records can never be
  read as part of the log. Opening for writing first sets the file to the replayed page count
  and always syncs, which also makes the replayed records durable before any page is written
  in place from them, and before a later batch record claims they are durable.
- **Truncation:** checkpoints (item 7) truncate the head by moving `log_head_page` and the tail
  by starting a segment with a higher sequence number.
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
  room for partition bounds. Until it exists, the superblock's `catalog_root` names the root
  of the one tree a file holds.
- **Key scheme 0**, the only scheme in v0.2.0, is the raw key compared as unsigned bytes. The
  empty key is a valid key, and the smallest.
- **Tenant and shard prefixes (D20):** a later scheme encodes `(tenant, shard, key)` into the
  same byte string, preserving order, and sets an incompatible feature bit.
- **Prefix truncation:** B+tree pages keep low and high fence keys and store the prefix those
  keys share only once. So an unused prefix costs nothing, and a used one costs about nothing
  per entry.

### B+tree pages (types 2 and 3, layout 1)
A tree is a B+tree (D77, D82): values live in leaf pages (type 3), and inner pages (type 2)
hold separator keys and child page numbers. Both types share one layout. After the 16-byte
page header comes a 32-byte tree header:

| Offset | Size | Field | Meaning |
|-------:|-----:|-------|---------|
| 16 | 2 | `count` | entries on the page: slots in the slot array |
| 18 | 2 | `prefix_len` | bytes every key on the page starts with, stored once: exactly the longest common prefix of the two fence keys, and 0 without a high fence key |
| 20 | 2 | `low_len` | length of the low fence key |
| 22 | 2 | `high_len` | length of the high fence key; 0 means the page has no upper bound |
| 24 | 1 | `level` | 0 for a leaf; a child is exactly one level below its parent. Below 48. |
| 25 | 3 | reserved | zero |
| 28 | 4 | `cell_start` | byte offset where the cell area starts, at most the page size |
| 32 | 8 | `right_child` | inner pages: the child holding the keys from the last separator up to the high fence key. Leaves: 0. |
| 40 | 4 | `garbage` | bytes in the cell area that no slot points to, left by replaced values |
| 44 | 4 | reserved | zero |
| 48 | | fence keys | the prefix (`prefix_len` bytes), the rest of the low fence key (`low_len − prefix_len`), the rest of the high fence key (`high_len − prefix_len`, none without one) |
| after them | 2 × `count` | slot array | the byte offset of each entry's cell, in increasing key order |

Free space runs from the end of the slot array to `cell_start`; the cells fill the rest of the
page to its end, in any order. A cell stores its key without the page's prefix:

| Page | Cell |
|------|------|
| leaf | key length (2), value length (2), key, value |
| inner | key length (2), child page number (8), key |

- **Key ranges.** A page holds the keys from its low fence key (included) up to its high fence
  key (excluded). Inner entry *i* names the child for keys below its separator and at or
  above the previous separator (or the page's low fence key); `right_child` takes the rest.
  So a child's fence keys are exactly the bounds its parent gives it. The root has an empty
  low fence key and no high fence key, and its page number never changes: a root split moves
  its entries into two new pages and turns it into an inner page one level up.
- **Separators.** A leaf split separates the halves by the shortest key above the last key of
  the left half and at most the first key of the right half: the first key of the right
  half, cut one byte after where it first differs from the last key of the left. An inner
  split moves its middle separator up.
- **Limits.** A key is at most `page_size / 8` bytes, and a key and value together at most
  `3 × page_size / 16 − 18` bytes, so four of the largest entries and two of the longest fence
  keys fill a leaf exactly (512 and 750 bytes with 4 KiB pages; 8,192 and 12,270 with 64 KiB
  pages). A longer key or entry is refused with an error. The value length's top bit is
  reserved for values stored outside the tree (item 11); it is zero in layout 1.
- **Validity.** A reader refuses a page that breaks any of these rules: a type that does not
  match its level (type 3 exactly at level 0), a layout other than 1, a level of 48 or more,
  reserved bytes that are not zero, `right_child` of 0 on an inner page or not 0 on a leaf,
  a fence key longer than the key limit, a `prefix_len` that is not the fence keys' common
  prefix, fence keys out of order, a slot array that overlaps the cells, a slot pointing
  outside the cell area, a cell running past the page end, cells that overlap, cell bytes and
  `garbage` that don't add up to the cell area, keys not strictly increasing, keys outside the
  fence keys (an inner page's separators must be strictly above its low fence key), sizes
  over the limits, a value length with its top bit set, and a child page number of 0. Each
  step down the tree also checks that the child is one level below its parent and that the
  key sought lies within the child's fence keys, which bounds every walk by the root's level.
  `eggdb check` also checks that every child's fence keys equal the bounds its parent gives
  it. Siblings' key ranges never overlap and are never empty, so no two children get the same
  bounds, and a page reached twice (a cycle or a shared child) fails that check.
- **An inner page may hold no separators**, only `right_child`: the fallback of an inner split
  can leave one, and the format allows it.
- **Deletes and merges (D83).** A page may hold any number of entries, none included: a
  delete removes the entry's slot and counts its cell as `garbage`, and a page left under a
  quarter full is merged with the page beside it under the same parent only when the two fit
  in one page with the prefix of their joined fence keys, which can be shorter than either
  page's. So an empty leaf can stay next to a full one. The root is the exception: an inner
  root always holds at least one separator, because a root left with a single child takes
  over that child's entries, and `eggdb check` refuses an inner root without one.

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
| `Full` (default) | its log records are synced; then its pages are written in place | lose nothing that was committed |
| `Normal` | its log records are written. Its pages are served from the log and written in place only after the next sync: an explicit `sync`, a change of mode, or a commit that would leave 1 MiB of log or more than 1024 pages waiting, which syncs before it returns. | lose the latest commits, never corrupt |
| `Off` | its records and pages are written; nothing is synced except a new file size, as below | corrupt the file (for bulk loads and tests) |

In every mode, a commit that grows the file by more pages than it logs images (pages allocated
and never written, or allocated and freed again in the same transaction) syncs the file's new
size before writing its records. Otherwise a power loss could keep the commit record but lose
the size, and the page count it claims would exceed what the file can prove (see "Torn tail
or corruption"), so the file would be refused instead of opening at the previous commit.

**Group commit.** A sync covers every commit written before it, and a sync of a log already
durable up to that point does nothing. With one writer, as in v0.2.0, commits under `Normal`
followed by one `sync` share it; the same "sync up to this LSN" step is where several writers
will wait for one leader later.

On Apple's systems, a sync that must reach the disk uses `F_FULLFSYNC`. The standard library's
`File::sync_data` and `File::sync_all` already do that on Apple targets (checked in the source
of the pinned toolchain, 1.99.1), so the platform layer adds nothing.

## Locking
- An open for writing takes an exclusive lock on the database file itself, through the platform
  layer:
  - `flock` on Linux, Apple's systems and the BSDs, whose locks belong to the open file
    description;
  - `LockFileEx` on Windows, on one byte at offset 2^62, past any data.
- A second open fails at once, even from the same process.
- A read-only open takes a shared lock.
- The lock disappears when the process ends, so there are no stale locks.
- Only on WASI, which has no file locks, does a file open unlocked.

## Compatibility
- Every later release reads version-1 files.
- A change that an older release would misread needs a feature flag (see "Feature flags").
  Within a page type, `layout` versions the body.
- `format_version` rises only for a change that feature flags can't express.
- The v0.2.0 sample file, committed to the repository, is opened and checked by CI on every
  change from then on (D20, D35).
