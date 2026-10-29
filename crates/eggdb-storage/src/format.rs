use crate::bytes::{read_u16, read_u32, read_u64, write_u16, write_u32, write_u64};
use crate::crc32c::{crc32c, crc32c_append};
use crate::error::FormatError;

/// The first eight bytes of every EggDB database file. The `\r\n`, `\x1A` and `\n` catch a file
/// mangled by a text-mode transfer, as PNG's signature does.
pub const MAGIC: [u8; 8] = *b"EggDB\r\x1A\n";

/// The format version this release writes and the newest it reads.
pub const FORMAT_VERSION: u16 = 1;

/// The size of each of the two header slots at the start of the file, whatever the page size.
pub const HEADER_SLOT_SIZE: usize = 4096;

/// The bytes at the start of each header slot that the superblock uses; the checksum covers
/// them and the rest of the slot is zero.
pub const SUPERBLOCK_SIZE: usize = 512;

/// The size of the header at the start of every page but the header slots.
pub const PAGE_HEADER_SIZE: usize = 16;

/// The bits of a page number reserved for a volume number; they are zero in this format.
pub const PAGE_VOLUME_MASK: u64 = 0xFFFF << 48;

/// How many recycled log segments the superblock remembers.
pub const RECYCLED_SEGMENTS: usize = 7;

/// The page number that means "no page", used for empty lists and absent roots. Page 0 always
/// holds the first header slot, so it never names a data page.
pub const NO_PAGE: u64 = 0;

/// The longest a log segment may be, in bytes: 64 MiB.
pub const MAX_LOG_SEGMENT_BYTES: u64 = 64 << 20;

const SUPERBLOCK_CHECKSUM_AT: usize = 8;
const LOG_SEQUENCE_RESERVED_AT: usize = 192;

/// The size of a page: a power of two from 4 KiB to 64 KiB, chosen when a file is created and
/// fixed for its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageSize {
    log2: u8,
}

impl PageSize {
    /// The smallest page size, as a base-two logarithm: 4 KiB.
    pub const MIN_LOG2: u8 = 12;
    /// The largest page size, as a base-two logarithm: 64 KiB.
    pub const MAX_LOG2: u8 = 16;
    /// The page size new files get unless asked otherwise: 4 KiB.
    pub const DEFAULT: Self = Self {
        log2: Self::MIN_LOG2,
    };

    /// The page size `2^log2` bytes, refused unless it lies between [`PageSize::MIN_LOG2`] and
    /// [`PageSize::MAX_LOG2`].
    pub fn from_log2(log2: u8) -> Result<Self, FormatError> {
        if (Self::MIN_LOG2..=Self::MAX_LOG2).contains(&log2) {
            Ok(Self { log2 })
        } else {
            Err(FormatError::InvalidPageSize { log2 })
        }
    }

    /// The size as a base-two logarithm.
    pub fn log2(self) -> u8 {
        self.log2
    }

    /// The size in bytes.
    pub fn bytes(self) -> usize {
        1 << self.log2
    }

    /// The first page that holds data rather than the header slots: page 2 for 4 KiB pages,
    /// page 1 for every larger size.
    pub fn first_data_page(self) -> u64 {
        (2 * HEADER_SLOT_SIZE).div_ceil(self.bytes()) as u64
    }
}

/// What a page holds, stored in the byte after its checksum.
///
/// Byte 0 is never a valid type, so a page that was never written is refused. Bytes from
/// `0x10` up are reserved for later structures, such as immutable sorted runs and columnar
/// pages, which arrive behind an incompatible feature flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageType {
    /// A page of the free list, holding the numbers of free pages.
    FreeList,
    /// An inner page of a B+tree, holding separator keys and child page numbers.
    BTreeInner,
    /// A leaf page of a B+tree, holding keys and values.
    BTreeLeaf,
    /// A page holding part of a value too large for a leaf.
    Overflow,
    /// The first page of a segment of the write-ahead log.
    LogSegment,
}

impl PageType {
    /// The byte that stores this type.
    pub fn to_byte(self) -> u8 {
        match self {
            Self::FreeList => 1,
            Self::BTreeInner => 2,
            Self::BTreeLeaf => 3,
            Self::Overflow => 4,
            Self::LogSegment => 5,
        }
    }

    /// The type stored as `byte`, or `None` for a byte this release does not know.
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::FreeList),
            2 => Some(Self::BTreeInner),
            3 => Some(Self::BTreeLeaf),
            4 => Some(Self::Overflow),
            5 => Some(Self::LogSegment),
            _ => None,
        }
    }
}

/// The 16-byte header at the start of every page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageHeader {
    /// What the page holds.
    pub page_type: PageType,
    /// The layout version of this page type, so one type can change its layout behind a
    /// feature flag without a new type byte.
    pub layout: u8,
    /// Bits reserved for per-page compression and encryption; zero in this format, and refused
    /// otherwise.
    pub flags: u16,
    /// The position in the write-ahead log of the last change to this page.
    pub lsn: u64,
}

/// What an open of a file asks for, which decides how its unknown feature flags are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Reading only.
    ReadOnly,
    /// Reading and writing.
    ReadWrite,
}

/// The feature flags of a file, in three sets with different rules for a release that finds a
/// bit it does not know, as ext4 does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Features {
    /// Features an older release may ignore entirely.
    pub compatible: u64,
    /// Features an older release may read past but must not write: it opens the file
    /// read-only.
    pub read_only_compatible: u64,
    /// Features an older release cannot read: it refuses the file.
    pub incompatible: u64,
}

impl Features {
    /// The compatible features this release knows: none yet.
    pub const KNOWN_COMPATIBLE: u64 = 0;
    /// The read-only-compatible bit of a file that has held keys with a time to live (D140):
    /// a release that does not know it would show expired keys and could write keys without
    /// their deadlines, so it may only read the file.
    pub const EXPIRING_KEYS: u64 = 1 << 0;
    /// The read-only-compatible features this release knows: [`Features::EXPIRING_KEYS`].
    pub const KNOWN_READ_ONLY_COMPATIBLE: u64 = Self::EXPIRING_KEYS;
    /// The incompatible features this release knows: none yet.
    pub const KNOWN_INCOMPATIBLE: u64 = 0;

    /// Whether this release may open a file with these features for `access`. Unknown
    /// compatible bits are ignored, unknown read-only-compatible bits refuse a write, and
    /// unknown incompatible bits refuse any open.
    pub fn check(&self, access: Access) -> Result<(), FormatError> {
        let incompatible = self.incompatible & !Self::KNOWN_INCOMPATIBLE;
        if incompatible != 0 {
            return Err(FormatError::UnsupportedFeatures { bits: incompatible });
        }
        let read_only = self.read_only_compatible & !Self::KNOWN_READ_ONLY_COMPATIBLE;
        if access == Access::ReadWrite && read_only != 0 {
            return Err(FormatError::ReadOnlyFeatures { bits: read_only });
        }
        Ok(())
    }

    /// The features in `self` or in `other`.
    pub fn union(self, other: Self) -> Self {
        Self {
            compatible: self.compatible | other.compatible,
            read_only_compatible: self.read_only_compatible | other.read_only_compatible,
            incompatible: self.incompatible | other.incompatible,
        }
    }
}

/// The identity of one database file, chosen at random when the file is created.
///
/// It seeds every page checksum, so a page copied from another file fails its checksum even
/// when it is otherwise intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub [u8; 16]);

impl FileId {
    /// A new identity drawn from the operating system's randomness, through the seeds of the
    /// standard library's hash maps.
    pub fn random() -> Self {
        use std::hash::{BuildHasher, RandomState};
        let mut bytes = [0u8; 16];
        let state = RandomState::new();
        bytes[..8].copy_from_slice(&state.hash_one(0u8).to_le_bytes());
        bytes[8..].copy_from_slice(&state.hash_one(1u8).to_le_bytes());
        Self(bytes)
    }
}

/// The contents of one header slot: where everything else in the file starts.
///
/// The file keeps two slots, at byte 0 and byte [`HEADER_SLOT_SIZE`]. Each update writes the
/// slot the old one is not in, with the generation raised by one, so a crash during the write
/// leaves the other slot intact. A superblock with an even generation belongs in slot 0 and an
/// odd one in slot 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Superblock {
    /// The page size of the file.
    pub page_size: PageSize,
    /// Raised by one on every header update; the valid slot with the higher generation wins.
    pub generation: u64,
    /// The file's feature flags.
    pub features: Features,
    /// The file's identity.
    pub file_id: FileId,
    /// How many pages the file holds, header pages included.
    pub page_count: u64,
    /// The first page of the free list, or [`NO_PAGE`].
    pub free_list_head: u64,
    /// How many pages the free list holds.
    pub free_page_count: u64,
    /// The root page of the catalog tree, which maps tree identities to their roots, or
    /// [`NO_PAGE`] in an empty file.
    pub catalog_root: u64,
    /// The log position from which recovery replays the write-ahead log.
    pub checkpoint_lsn: u64,
    /// The first page of the oldest live log segment, or [`NO_PAGE`].
    pub log_head_page: u64,
    /// The sequence number of the oldest live log segment.
    pub log_head_sequence: u64,
    /// The highest hybrid-clock timestamp handed out before the last checkpoint.
    pub clock_high_water: u64,
    /// The length of a log segment, in pages.
    pub log_segment_pages: u32,
    /// Log segments emptied by checkpoints and kept for reuse before the file grows, each a
    /// first page; unused entries are [`NO_PAGE`].
    pub recycled_segments: [u64; RECYCLED_SEGMENTS],
    /// The highest log segment sequence number a writer may have used: a writer that opens the
    /// file starts above it, and writes the header again before it uses a higher one, so no
    /// sequence number is ever used twice.
    pub log_sequence_reserved: u64,
}

impl Superblock {
    /// The slot, 0 or 1, this superblock is written to.
    pub fn slot(&self) -> usize {
        (self.generation & 1) as usize
    }

    pub(crate) fn is_copied_in(&self, other_slot: &[u8]) -> bool {
        Self::decode(other_slot, 1 - self.slot()).is_ok_and(|other| {
            Self {
                generation: self.generation,
                ..other
            } == *self
        })
    }

    /// Encodes the superblock into a whole header slot, checksum included. Refuses a
    /// superblock that [`Superblock::decode`] would refuse, so an invalid header never reaches
    /// the file.
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        self.check_fields()?;
        Ok(self.encode_unchecked())
    }

    fn encode_unchecked(&self) -> Vec<u8> {
        let mut slot = vec![0u8; HEADER_SLOT_SIZE];
        slot[..8].copy_from_slice(&MAGIC);
        write_u16(&mut slot, 12, FORMAT_VERSION);
        slot[14] = self.page_size.log2();
        write_u64(&mut slot, 16, self.generation);
        write_u64(&mut slot, 24, self.features.compatible);
        write_u64(&mut slot, 32, self.features.read_only_compatible);
        write_u64(&mut slot, 40, self.features.incompatible);
        slot[48..64].copy_from_slice(&self.file_id.0);
        write_u64(&mut slot, 64, self.page_count);
        write_u64(&mut slot, 72, self.free_list_head);
        write_u64(&mut slot, 80, self.free_page_count);
        write_u64(&mut slot, 88, self.catalog_root);
        write_u64(&mut slot, 96, self.checkpoint_lsn);
        write_u64(&mut slot, 104, self.log_head_page);
        write_u64(&mut slot, 112, self.log_head_sequence);
        write_u64(&mut slot, 120, self.clock_high_water);
        write_u32(&mut slot, 128, self.log_segment_pages);
        for (index, &page) in self.recycled_segments.iter().enumerate() {
            write_u64(&mut slot, 136 + 8 * index, page);
        }
        write_u64(
            &mut slot,
            LOG_SEQUENCE_RESERVED_AT,
            self.log_sequence_reserved,
        );
        let checksum = superblock_checksum(&slot);
        write_u32(&mut slot, SUPERBLOCK_CHECKSUM_AT, checksum);
        slot
    }

    /// Decodes the header slot `slot_index` (0 or 1), checking its length, magic, checksum,
    /// version, slot and fields. Feature flags are left to [`Features::check`], so a tool can still
    /// report on a file it may not open.
    pub fn decode(slot: &[u8], slot_index: usize) -> Result<Self, FormatError> {
        check_slot(slot, slot_index)?;
        let major = read_u16(slot, 12);
        if major > FORMAT_VERSION || major == 0 {
            return Err(FormatError::UnsupportedVersion { major });
        }
        let page_size = PageSize::from_log2(slot[14])?;
        if slot[15] != 0 {
            return Err(FormatError::InvalidField {
                field: "page_reserved_tail",
            });
        }
        let mut recycled_segments = [NO_PAGE; RECYCLED_SEGMENTS];
        for (index, page) in recycled_segments.iter_mut().enumerate() {
            *page = read_u64(slot, 136 + 8 * index);
        }
        let superblock = Self {
            page_size,
            generation: read_u64(slot, 16),
            features: Features {
                compatible: read_u64(slot, 24),
                read_only_compatible: read_u64(slot, 32),
                incompatible: read_u64(slot, 40),
            },
            file_id: FileId(slot[48..64].try_into().unwrap_or([0; 16])),
            page_count: read_u64(slot, 64),
            free_list_head: read_u64(slot, 72),
            free_page_count: read_u64(slot, 80),
            catalog_root: read_u64(slot, 88),
            checkpoint_lsn: read_u64(slot, 96),
            log_head_page: read_u64(slot, 104),
            log_head_sequence: read_u64(slot, 112),
            clock_high_water: read_u64(slot, 120),
            log_segment_pages: read_u32(slot, 128),
            recycled_segments,
            log_sequence_reserved: read_u64(slot, LOG_SEQUENCE_RESERVED_AT),
        };
        if superblock.slot() != slot_index {
            return Err(FormatError::InvalidField {
                field: "generation",
            });
        }
        superblock.check_fields()?;
        Ok(superblock)
    }

    pub(crate) fn check(&self) -> Result<(), FormatError> {
        self.check_fields()
    }

    fn check_fields(&self) -> Result<(), FormatError> {
        let first_data_page = self.page_size.first_data_page();
        if self.page_count < first_data_page || self.page_count & PAGE_VOLUME_MASK != 0 {
            return Err(FormatError::InvalidField {
                field: "page_count",
            });
        }
        if self.free_page_count > self.page_count - first_data_page {
            return Err(FormatError::InvalidField {
                field: "free_page_count",
            });
        }
        let pages = [
            ("free_list_head", self.free_list_head),
            ("catalog_root", self.catalog_root),
            ("log_head_page", self.log_head_page),
        ];
        let recycled = self
            .recycled_segments
            .iter()
            .map(|&page| ("recycled_segments", page));
        for (field, page) in pages.into_iter().chain(recycled) {
            if page != NO_PAGE && (page < first_data_page || page >= self.page_count) {
                return Err(FormatError::InvalidField { field });
            }
        }
        if (self.free_list_head == NO_PAGE) != (self.free_page_count == 0) {
            return Err(FormatError::InvalidField {
                field: "free_list_head",
            });
        }
        let segment_bytes = u64::from(self.log_segment_pages) * self.page_size.bytes() as u64;
        let has_log = self.log_head_page != NO_PAGE
            || self.recycled_segments.iter().any(|&page| page != NO_PAGE);
        let too_small_for_a_log =
            has_log && !crate::log::segment_holds_a_page(self.page_size, self.log_segment_pages);
        if self.log_segment_pages == 0
            || segment_bytes > MAX_LOG_SEGMENT_BYTES
            || too_small_for_a_log
        {
            return Err(FormatError::InvalidField {
                field: "log_segment_pages",
            });
        }
        let segment_end = |first: u64| first + u64::from(self.log_segment_pages);
        if self.log_head_page != NO_PAGE && segment_end(self.log_head_page) > self.page_count {
            return Err(FormatError::InvalidField {
                field: "log_head_page",
            });
        }
        let recycled_past_the_end = self
            .recycled_segments
            .iter()
            .any(|&first| first != NO_PAGE && segment_end(first) > self.page_count);
        if recycled_past_the_end || self.log_segments_overlap() {
            return Err(FormatError::InvalidField {
                field: "recycled_segments",
            });
        }
        let in_a_log_segment = |page: u64| {
            std::iter::once(self.log_head_page)
                .chain(self.recycled_segments)
                .filter(|&first| first != NO_PAGE)
                .any(|first| (first..segment_end(first)).contains(&page))
        };
        for (field, page) in [
            ("catalog_root", self.catalog_root),
            ("free_list_head", self.free_list_head),
        ] {
            if page != NO_PAGE && in_a_log_segment(page) {
                return Err(FormatError::InvalidField { field });
            }
        }
        if self.log_head_page != NO_PAGE && self.log_sequence_reserved < self.log_head_sequence {
            return Err(FormatError::InvalidField {
                field: "log_sequence_reserved",
            });
        }
        Ok(())
    }

    fn log_segments_overlap(&self) -> bool {
        let mut firsts = [NO_PAGE; RECYCLED_SEGMENTS + 1];
        firsts[0] = self.log_head_page;
        firsts[1..].copy_from_slice(&self.recycled_segments);
        firsts.sort_unstable();
        firsts
            .windows(2)
            .filter(|pair| pair[0] != NO_PAGE)
            .any(|pair| pair[1] - pair[0] < u64::from(self.log_segment_pages))
    }

    /// Picks the current superblock from the two header slots: the slot with the higher
    /// generation among those whose magic and checksum hold, then checks it fully.
    ///
    /// A slot that fails its checksum is treated as a write that a crash interrupted, and the
    /// other slot is used; [`HeaderChoice::damaged_slot`] says so, because a slot damaged after
    /// it was synced also looks like that, and the caller should report it. A slot whose
    /// checksum holds but whose contents are refused (a newer version, a superblock in the
    /// wrong slot, a forbidden field) refuses the file rather than falling back to the older
    /// slot, so a release never silently opens an older state of a file a newer release wrote.
    /// Two intact slots with the same generation refuse the file too.
    pub fn choose(slot0: &[u8], slot1: &[u8]) -> Result<HeaderChoice, FormatError> {
        let intact = (check_slot(slot0, 0).is_ok(), check_slot(slot1, 1).is_ok());
        let has_magic = |slot: &[u8]| slot.get(..8) == Some(&MAGIC[..]);
        let newest = match intact {
            (true, true) => match read_u64(slot1, 16).cmp(&read_u64(slot0, 16)) {
                std::cmp::Ordering::Greater => 1,
                std::cmp::Ordering::Less => 0,
                std::cmp::Ordering::Equal => {
                    return Err(FormatError::InvalidField {
                        field: "generation",
                    });
                }
            },
            (true, false) => 0,
            (false, true) => 1,
            (false, false) => {
                return Err(if !has_magic(slot0) && !has_magic(slot1) {
                    FormatError::NotADatabase
                } else {
                    FormatError::NoValidHeader
                });
            }
        };
        let superblock = if newest == 0 {
            Self::decode(slot0, 0)?
        } else {
            Self::decode(slot1, 1)?
        };
        let other = 1 - newest;
        let other_slot = if other == 0 { slot0 } else { slot1 };
        let damaged_slot =
            (has_magic(other_slot) && check_slot(other_slot, other).is_err()).then_some(other);
        Ok(HeaderChoice {
            superblock,
            damaged_slot,
        })
    }
}

/// The superblock [`Superblock::choose`] picked, and whether the other slot was damaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HeaderChoice {
    /// The current superblock.
    pub superblock: Superblock,
    /// The other slot, when it has the magic bytes but fails its checksum: torn by a crash
    /// during a header update, or damaged since. `None` when the other slot is intact or was
    /// never written.
    pub damaged_slot: Option<usize>,
}

fn check_slot(slot: &[u8], slot_index: usize) -> Result<(), FormatError> {
    if slot.len() != HEADER_SLOT_SIZE {
        return Err(FormatError::WrongLength {
            expected: HEADER_SLOT_SIZE,
            actual: slot.len(),
        });
    }
    if slot[..8] != MAGIC {
        return Err(FormatError::NotADatabase);
    }
    if read_u32(slot, SUPERBLOCK_CHECKSUM_AT) != superblock_checksum(slot) {
        return Err(FormatError::HeaderChecksumMismatch { slot: slot_index });
    }
    Ok(())
}

fn superblock_checksum(slot: &[u8]) -> u32 {
    crc32c_append(
        crc32c(&slot[..SUPERBLOCK_CHECKSUM_AT]),
        &slot[SUPERBLOCK_CHECKSUM_AT + 4..SUPERBLOCK_SIZE],
    )
}

/// Writes and verifies page checksums for one file.
///
/// A page's checksum is CRC-32C over every byte after the checksum itself, seeded with the
/// file's [`FileId`] and the page number. A page written to the wrong place, or copied from
/// another file, therefore fails its checksum like a damaged one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageChecksum {
    file_seed: u32,
    page_size: PageSize,
}

impl PageChecksum {
    /// The checksums of the file `file_id`, whose pages are `page_size` long.
    pub fn new(file_id: FileId, page_size: PageSize) -> Self {
        Self {
            file_seed: crc32c(&file_id.0),
            page_size,
        }
    }

    /// Writes `header` into the start of `page` and then the checksum of the whole page as
    /// page number `page_id`. Refuses a buffer of the wrong length, a page number in the
    /// header slots or with volume bits set, and flags other than zero.
    pub fn write(
        &self,
        page: &mut [u8],
        page_id: u64,
        header: &PageHeader,
    ) -> Result<(), FormatError> {
        self.check_length(page)?;
        self.check_page_id(page_id)?;
        if header.flags != 0 {
            return Err(FormatError::InvalidField { field: "flags" });
        }
        page[4] = header.page_type.to_byte();
        page[5] = header.layout;
        write_u16(page, 6, header.flags);
        write_u64(page, 8, header.lsn);
        let checksum = self.checksum(page, page_id);
        write_u32(page, 0, checksum);
        Ok(())
    }

    /// Checks that `page` is page number `page_id` of this file, intact, of a known type and
    /// with no flags set, and returns its header.
    pub fn verify(&self, page: &[u8], page_id: u64) -> Result<PageHeader, FormatError> {
        self.check_length(page)?;
        self.check_page_id(page_id)?;
        if read_u32(page, 0) != self.checksum(page, page_id) {
            return Err(FormatError::ChecksumMismatch { page: page_id });
        }
        let page_type = PageType::from_byte(page[4]).ok_or(FormatError::UnknownPageType {
            page: page_id,
            found: page[4],
        })?;
        let flags = read_u16(page, 6);
        if flags != 0 {
            return Err(FormatError::InvalidField { field: "flags" });
        }
        Ok(PageHeader {
            page_type,
            layout: page[5],
            flags,
            lsn: read_u64(page, 8),
        })
    }

    fn checksum(&self, page: &[u8], page_id: u64) -> u32 {
        let page_seed = crc32c_append(self.file_seed, &page_id.to_le_bytes());
        crc32c_append(page_seed, &page[4..])
    }

    fn check_length(&self, page: &[u8]) -> Result<(), FormatError> {
        if page.len() == self.page_size.bytes() {
            Ok(())
        } else {
            Err(FormatError::WrongLength {
                expected: self.page_size.bytes(),
                actual: page.len(),
            })
        }
    }

    fn check_page_id(&self, page_id: u64) -> Result<(), FormatError> {
        if page_id & PAGE_VOLUME_MASK == 0 && page_id >= self.page_size.first_data_page() {
            Ok(())
        } else {
            Err(FormatError::InvalidField { field: "page_id" })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Access, FileId, HEADER_SLOT_SIZE, NO_PAGE, PageChecksum, PageHeader, PageSize, PageType,
        SUPERBLOCK_SIZE, Superblock,
    };
    use crate::error::FormatError;
    use crate::format::Features;
    use crate::test_rng::TestRng;

    const SECTOR: usize = 512;

    type Damage = fn(&mut Superblock);

    fn superblock(generation: u64) -> Superblock {
        Superblock {
            page_size: PageSize::DEFAULT,
            generation,
            features: Features::default(),
            file_id: FileId(*b"0123456789abcdef"),
            page_count: 60,
            free_list_head: 7,
            free_page_count: 3,
            catalog_root: 2,
            checkpoint_lsn: 4096 * generation,
            log_head_page: 16,
            log_head_sequence: generation,
            clock_high_water: 0x1234_5678,
            log_segment_pages: 16,
            recycled_segments: [40, NO_PAGE, NO_PAGE, NO_PAGE, NO_PAGE, NO_PAGE, NO_PAGE],
            log_sequence_reserved: generation + 9,
        }
    }

    fn leaf(lsn: u64) -> PageHeader {
        PageHeader {
            page_type: PageType::BTreeLeaf,
            layout: 1,
            flags: 0,
            lsn,
        }
    }

    fn checksums() -> PageChecksum {
        PageChecksum::new(FileId(*b"0123456789abcdef"), PageSize::DEFAULT)
    }

    fn written_page(rng: &mut TestRng, page_id: u64, lsn: u64) -> Vec<u8> {
        let mut page = vec![0u8; PageSize::DEFAULT.bytes()];
        rng.fill(&mut page);
        checksums().write(&mut page, page_id, &leaf(lsn)).unwrap();
        page
    }

    #[test]
    fn a_superblock_survives_encoding_in_its_own_slot() {
        for generation in [0, 1, 2, 77] {
            let original = superblock(generation);
            let slot = original.encode().unwrap();
            assert_eq!(slot.len(), HEADER_SLOT_SIZE);
            assert_eq!(Superblock::decode(&slot, original.slot()), Ok(original));
            assert_eq!(
                Superblock::decode(&slot, 1 - original.slot()),
                Err(FormatError::InvalidField {
                    field: "generation"
                })
            );
        }
    }

    #[test]
    fn every_bit_flip_in_a_superblock_is_refused() {
        let slot = superblock(4).encode().unwrap();
        for bit in 0..SUPERBLOCK_SIZE * 8 {
            let mut damaged = slot.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            assert!(Superblock::decode(&damaged, 0).is_err(), "bit {bit}");
        }
    }

    #[test]
    fn the_newest_intact_slot_wins_and_a_torn_one_falls_back() {
        let old = superblock(6);
        let new = superblock(7);
        let (old_slot, new_slot) = (old.encode().unwrap(), new.encode().unwrap());
        assert_eq!(
            Superblock::choose(&old_slot, &new_slot).map(|choice| choice.superblock),
            Ok(new)
        );
        let mut rng = TestRng::new(20);
        for _ in 0..2000 {
            let mut torn = vec![0u8; HEADER_SLOT_SIZE];
            rng.fill(&mut torn);
            for sector in 0..HEADER_SLOT_SIZE / SECTOR {
                if rng.below(2) == 0 {
                    let range = sector * SECTOR..(sector + 1) * SECTOR;
                    torn[range.clone()].copy_from_slice(&new_slot[range]);
                }
            }
            let choice = Superblock::choose(&old_slot, &torn).unwrap();
            assert!(choice.superblock == old || choice.superblock == new);
            if choice.superblock == old && torn[..8] == super::MAGIC {
                assert_eq!(choice.damaged_slot, Some(1));
            }
        }
    }

    #[test]
    fn a_first_header_with_a_blank_second_slot_opens_and_reports_no_damage() {
        let blank = vec![0u8; HEADER_SLOT_SIZE];
        let choice = Superblock::choose(&superblock(0).encode().unwrap(), &blank).unwrap();
        assert_eq!(choice.superblock, superblock(0));
        assert_eq!(choice.damaged_slot, None);
    }

    #[test]
    fn a_header_update_cut_off_after_any_sector_opens_the_old_or_the_new_state() {
        let old = superblock(10);
        let new = superblock(11);
        let previous = superblock(9).encode().unwrap();
        let (old_slot, new_slot) = (old.encode().unwrap(), new.encode().unwrap());
        for sectors_written in 0..=HEADER_SLOT_SIZE / SECTOR {
            let mut slot1 = previous.clone();
            let written = sectors_written * SECTOR;
            slot1[..written].copy_from_slice(&new_slot[..written]);
            let choice = Superblock::choose(&old_slot, &slot1).unwrap();
            if written >= SUPERBLOCK_SIZE {
                assert_eq!(choice.superblock, new);
            } else {
                assert_eq!(choice.superblock, old);
            }
        }
    }

    #[test]
    fn two_slots_with_the_same_generation_refuse_the_file() {
        let slot = superblock(4).encode().unwrap();
        assert_eq!(
            Superblock::choose(&slot, &slot),
            Err(FormatError::InvalidField {
                field: "generation"
            })
        );
    }

    #[test]
    fn a_newer_version_in_the_newest_slot_refuses_the_file_instead_of_falling_back() {
        let old_slot = superblock(2).encode().unwrap();
        let mut newer = superblock(3).encode().unwrap();
        newer[12] = 2;
        let checksum = super::superblock_checksum(&newer);
        newer[8..12].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            Superblock::choose(&old_slot, &newer),
            Err(FormatError::UnsupportedVersion { major: 2 })
        );
    }

    #[test]
    fn files_without_a_valid_header_are_told_apart() {
        let zeros = vec![0u8; HEADER_SLOT_SIZE];
        assert_eq!(
            Superblock::choose(&zeros, &zeros),
            Err(FormatError::NotADatabase)
        );
        let mut damaged = superblock(0).encode().unwrap();
        damaged[100] ^= 1;
        assert_eq!(
            Superblock::choose(&damaged, &zeros),
            Err(FormatError::NoValidHeader)
        );
        assert_eq!(
            Superblock::decode(&damaged, 0),
            Err(FormatError::HeaderChecksumMismatch { slot: 0 })
        );
    }

    #[test]
    fn fields_pointing_outside_the_data_pages_are_refused() {
        let cases: [(&str, Damage); 16] = [
            ("page_count", |s| s.page_count = 1),
            ("page_count", |s| s.page_count = 1 << 48),
            ("recycled_segments", |s| s.recycled_segments[3] = 60),
            ("recycled_segments", |s| s.recycled_segments[0] = 50),
            ("recycled_segments", |s| s.recycled_segments[3] = 40),
            ("recycled_segments", |s| s.recycled_segments[0] = 30),
            ("recycled_segments", |s| s.recycled_segments[5] = 2),
            ("log_head_page", |s| s.log_head_page = 50),
            ("free_list_head", |s| s.free_list_head = 60),
            ("catalog_root", |s| s.catalog_root = 1),
            ("log_head_page", |s| s.log_head_page = u64::MAX),
            ("free_page_count", |s| s.free_page_count = 59),
            ("log_segment_pages", |s| s.log_segment_pages = 0),
            ("log_sequence_reserved", |s| {
                s.log_head_sequence = s.log_sequence_reserved + 1
            }),
            ("catalog_root", |s| s.catalog_root = 20),
            ("free_list_head", |s| s.free_list_head = 45),
        ];
        let mut huge_segments = superblock(0);
        huge_segments.log_segment_pages = (super::MAX_LOG_SEGMENT_BYTES / 4096) as u32 + 1;
        huge_segments.page_count = 1 << 40;
        assert_eq!(
            Superblock::decode(&huge_segments.encode_unchecked(), 0),
            Err(FormatError::InvalidField {
                field: "log_segment_pages"
            })
        );
        for (field, damage) in cases {
            let mut bad = superblock(0);
            damage(&mut bad);
            assert_eq!(
                Superblock::decode(&bad.encode_unchecked(), 0),
                Err(FormatError::InvalidField { field }),
                "{field}"
            );
        }
        let mut empty_list = superblock(0);
        empty_list.free_list_head = NO_PAGE;
        assert!(Superblock::decode(&empty_list.encode_unchecked(), 0).is_err());
        assert!(empty_list.encode().is_err());
    }

    #[test]
    fn unknown_features_refuse_a_write_or_any_open_by_their_set() {
        let compatible = Features {
            compatible: 1 << 9,
            ..Features::default()
        };
        assert_eq!(compatible.check(Access::ReadWrite), Ok(()));
        let read_only = Features {
            read_only_compatible: 1 << 3,
            ..Features::default()
        };
        assert_eq!(read_only.check(Access::ReadOnly), Ok(()));
        assert_eq!(
            read_only.check(Access::ReadWrite),
            Err(FormatError::ReadOnlyFeatures { bits: 1 << 3 })
        );
        let incompatible = Features {
            incompatible: 1 << 63,
            ..Features::default()
        };
        let refused = Err(FormatError::UnsupportedFeatures { bits: 1 << 63 });
        assert_eq!(incompatible.check(Access::ReadOnly), refused);
        assert_eq!(incompatible.check(Access::ReadWrite), refused);
    }

    #[test]
    fn expiring_keys_are_a_known_read_only_compatible_bit_zero() {
        assert_eq!(Features::EXPIRING_KEYS, 1);
        let expiring = Features {
            read_only_compatible: Features::EXPIRING_KEYS,
            ..Features::default()
        };
        assert_eq!(expiring.check(Access::ReadWrite), Ok(()));
        let with_unknown = expiring.union(Features {
            read_only_compatible: 1 << 1,
            ..Features::default()
        });
        assert_eq!(with_unknown.read_only_compatible, 0b11);
        assert_eq!(
            with_unknown.check(Access::ReadWrite),
            Err(FormatError::ReadOnlyFeatures { bits: 1 << 1 })
        );
    }

    #[test]
    fn page_sizes_run_from_4_kib_to_64_kib() {
        assert_eq!(
            PageSize::from_log2(11),
            Err(FormatError::InvalidPageSize { log2: 11 })
        );
        assert_eq!(
            PageSize::from_log2(17),
            Err(FormatError::InvalidPageSize { log2: 17 })
        );
        let first_data_pages: Vec<u64> = (12..=16)
            .map(|log2| PageSize::from_log2(log2).unwrap().first_data_page())
            .collect();
        assert_eq!(first_data_pages, [2, 1, 1, 1, 1]);
    }

    #[test]
    fn a_page_survives_writing_and_verifying() {
        let page = written_page(&mut TestRng::new(1), 9, 1234);
        assert_eq!(checksums().verify(&page, 9), Ok(leaf(1234)));
    }

    #[test]
    fn ten_thousand_seeded_corruptions_are_all_refused() {
        let mut rng = TestRng::new(10_000);
        for case in 0..10_000 {
            let page_id = 2 + rng.below(1000) as u64;
            let page = written_page(&mut rng, page_id, case);
            let mut damaged = page.clone();
            match case % 3 {
                0 => {
                    for _ in 0..1 + rng.below(3) {
                        let bit = rng.below(damaged.len() * 8);
                        damaged[bit / 8] ^= 1 << (bit % 8);
                    }
                }
                1 => {
                    let newer = written_page(&mut rng, page_id, case + 1);
                    let sector = rng.below(damaged.len() / SECTOR) * SECTOR;
                    damaged[sector..sector + SECTOR]
                        .copy_from_slice(&newer[sector..sector + SECTOR]);
                }
                _ => {
                    let sector = rng.below(damaged.len() / SECTOR) * SECTOR;
                    damaged[sector..sector + SECTOR].fill(0);
                }
            }
            if damaged != page {
                assert_eq!(
                    checksums().verify(&damaged, page_id),
                    Err(FormatError::ChecksumMismatch { page: page_id }),
                    "case {case}"
                );
            }
        }
    }

    #[test]
    fn a_page_read_from_the_wrong_place_or_another_file_is_refused() {
        let page = written_page(&mut TestRng::new(2), 5, 1);
        assert_eq!(
            checksums().verify(&page, 6),
            Err(FormatError::ChecksumMismatch { page: 6 })
        );
        let other_file = PageChecksum::new(FileId(*b"fedcba9876543210"), PageSize::DEFAULT);
        assert_eq!(
            other_file.verify(&page, 5),
            Err(FormatError::ChecksumMismatch { page: 5 })
        );
    }

    #[test]
    fn a_page_never_written_or_of_an_unknown_type_is_refused() {
        let zeros = vec![0u8; PageSize::DEFAULT.bytes()];
        assert!(checksums().verify(&zeros, 3).is_err());
        let mut unknown = written_page(&mut TestRng::new(3), 3, 1);
        unknown[4] = 0x10;
        let checksum = checksums().checksum(&unknown, 3);
        unknown[..4].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            checksums().verify(&unknown, 3),
            Err(FormatError::UnknownPageType {
                page: 3,
                found: 0x10
            })
        );
    }

    #[test]
    fn page_flags_other_than_zero_are_refused() {
        let mut rng = TestRng::new(4);
        let mut page = written_page(&mut rng, 3, 1);
        let flagged = PageHeader {
            flags: 1,
            ..leaf(1)
        };
        let field = FormatError::InvalidField { field: "flags" };
        assert_eq!(checksums().write(&mut page, 3, &flagged), Err(field));
        page[6] = 1;
        let checksum = checksums().checksum(&page, 3);
        page[..4].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(checksums().verify(&page, 3), Err(field));
    }

    #[test]
    fn header_pages_volume_bits_and_wrong_lengths_are_refused() {
        let mut page = vec![0u8; PageSize::DEFAULT.bytes()];
        let checks = checksums();
        let field = Err(FormatError::InvalidField { field: "page_id" });
        assert_eq!(checks.write(&mut page, 1, &leaf(1)), field);
        assert_eq!(checks.write(&mut page, 1 << 48, &leaf(1)), field);
        assert_eq!(
            checks.verify(&page, 0),
            Err(FormatError::InvalidField { field: "page_id" })
        );
        assert_eq!(
            checks.verify(&page[1..], 2),
            Err(FormatError::WrongLength {
                expected: 4096,
                actual: 4095
            })
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_decoders() {
        let mut rng = TestRng::new(0xF022);
        let valid = superblock(8).encode().unwrap();
        let checks = checksums();
        for case in 0..10_000 {
            let mut slot = if case % 2 == 0 {
                valid.clone()
            } else {
                rng.bytes(HEADER_SLOT_SIZE)
            };
            for _ in 0..rng.below(16) {
                let at = rng.below(SUPERBLOCK_SIZE);
                slot[at] = rng.next_u64() as u8;
            }
            if case % 4 == 0 {
                let checksum = super::superblock_checksum(&slot);
                slot[8..12].copy_from_slice(&checksum.to_le_bytes());
            }
            let length = if case % 97 == 0 {
                rng.below(HEADER_SLOT_SIZE)
            } else {
                HEADER_SLOT_SIZE
            };
            let _ = Superblock::decode(&slot[..length], rng.below(3));
            let _ = Superblock::choose(&slot[..length], &valid);
            let _ = Superblock::choose(&valid, &slot[..length]);
            if let Ok(superblock) = Superblock::decode(&slot, superblock_slot(&slot)) {
                let _ = superblock.features.check(Access::ReadWrite);
            }
            let page_id = 2 + rng.below(1000) as u64;
            let _ = checks.verify(&slot[..length], page_id);
            if case % 3 == 0 {
                let checksum = checks.checksum(&slot, page_id);
                slot[..4].copy_from_slice(&checksum.to_le_bytes());
                let _ = checks.verify(&slot, page_id);
            }
        }
    }

    fn superblock_slot(slot: &[u8]) -> usize {
        (slot[16] & 1) as usize
    }
}
