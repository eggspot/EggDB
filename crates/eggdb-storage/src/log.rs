use crate::bytes::{read_u32, read_u64, write_u32, write_u64};
use crate::cells;
use crate::crc32c::{crc32c, crc32c_append};
use crate::error::{FormatError, StorageError};
use crate::format::{
    Features, FileId, PAGE_HEADER_SIZE, PAGE_VOLUME_MASK, PageChecksum, PageSize, PageType,
    Superblock,
};
use eggdb_platform::{ErrorKind, File};
use std::collections::BTreeMap;

pub(crate) const LOG_BLOCK: usize = 4096;
pub(crate) const RECORD_HEADER: usize = 10;
pub(crate) const SEGMENT_LAYOUT: u8 = 1;
const SEGMENT_SEQUENCE_AT: usize = PAGE_HEADER_SIZE;
const LINK_PAYLOAD: usize = 16;
const LINK_RECORD: usize = RECORD_HEADER + LINK_PAYLOAD;
const BATCH_PAYLOAD: usize = 8;
const IMAGE_PAGE_AT: usize = 8;
pub(crate) const COMMIT_PAYLOAD: usize = 9 * 8;
const SEGMENT_ROOM_BEYOND_A_PAGE: u64 = 8192;
const DEFAULT_SEGMENT_BYTES: usize = 64 << 10;

/// How long a commit waits before it returns, and so what a power loss can take (format.md,
/// "Durability modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Durability {
    /// A commit returns once its log records are synced: a power loss loses nothing that was
    /// committed. The default.
    #[default]
    Full,
    /// A commit returns once its log records are written, without a sync; its pages reach
    /// their places in the file only after the next sync, so a power loss can lose the latest
    /// commits but never corrupts the file. A sync happens when [`PageFile::sync`] is called,
    /// when the durability changes, and once a megabyte of log or a thousand pages wait for
    /// one.
    ///
    /// [`PageFile::sync`]: crate::PageFile::sync
    Normal,
    /// A commit writes its records and its pages and never syncs: a power loss can corrupt
    /// the file. For bulk loads and tests.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordType {
    Batch,
    Pad,
    PageImage,
    Commit,
    Link,
}

impl RecordType {
    fn to_byte(self) -> u8 {
        match self {
            Self::Batch => 1,
            Self::Pad => 2,
            Self::PageImage => 3,
            Self::Commit => 4,
            Self::Link => 5,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Batch),
            2 => Some(Self::Pad),
            3 => Some(Self::PageImage),
            4 => Some(Self::Commit),
            5 => Some(Self::Link),
            _ => None,
        }
    }
}

pub(crate) fn default_segment_pages(page_size: PageSize) -> u32 {
    (DEFAULT_SEGMENT_BYTES / page_size.bytes()).max(2) as u32
}

pub(crate) fn segment_holds_a_page(page_size: PageSize, segment_pages: u32) -> bool {
    let page_bytes = page_size.bytes() as u64;
    u64::from(segment_pages) * page_bytes >= page_bytes + SEGMENT_ROOM_BEYOND_A_PAGE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) first_page: u64,
    pub(crate) sequence: u64,
    pub(crate) start_lsn: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LogGeometry {
    page_size: PageSize,
    segment_pages: u32,
    file_seed: u32,
}

impl LogGeometry {
    pub(crate) fn new(file_id: FileId, page_size: PageSize, segment_pages: u32) -> Self {
        Self {
            page_size,
            segment_pages,
            file_seed: crc32c(&file_id.0),
        }
    }

    pub(crate) fn segment_pages(&self) -> u32 {
        self.segment_pages
    }

    fn segment_bytes(&self) -> usize {
        self.segment_pages as usize * self.page_size.bytes()
    }

    fn capacity(&self) -> u64 {
        (self.segment_bytes() - LOG_BLOCK) as u64
    }

    fn file_offset(&self, page: u64) -> u64 {
        page * self.page_size.bytes() as u64
    }

    fn record_seed(&self, sequence: u64, lsn: u64) -> u32 {
        let segment_seed = crc32c_append(self.file_seed, &sequence.to_le_bytes());
        crc32c_append(segment_seed, &lsn.to_le_bytes())
    }

    fn header_checksum(&self, first_page: u64, block: &[u8]) -> u32 {
        let seed = crc32c_append(self.file_seed, &first_page.to_le_bytes());
        crc32c_append(seed, &block[4..LOG_BLOCK])
    }

    pub(crate) fn encode_segment_header(&self, segment: &Segment, block: &mut [u8]) {
        block[..LOG_BLOCK].fill(0);
        block[4] = PageType::LogSegment.to_byte();
        block[5] = SEGMENT_LAYOUT;
        write_u64(block, 8, segment.start_lsn);
        write_u64(block, SEGMENT_SEQUENCE_AT, segment.sequence);
        let checksum = self.header_checksum(segment.first_page, block);
        write_u32(block, 0, checksum);
    }

    fn decode_segment_header(&self, first_page: u64, block: &[u8]) -> Option<Segment> {
        let valid = block.len() >= LOG_BLOCK
            && read_u32(block, 0) == self.header_checksum(first_page, block)
            && block[4] == PageType::LogSegment.to_byte()
            && block[5] == SEGMENT_LAYOUT
            && block[6] == 0
            && block[7] == 0;
        valid.then(|| Segment {
            first_page,
            sequence: read_u64(block, SEGMENT_SEQUENCE_AT),
            start_lsn: read_u64(block, 8),
        })
    }

    fn lsn_at(&self, segment: &Segment, offset: usize) -> u64 {
        segment
            .start_lsn
            .saturating_add(offset.saturating_sub(LOG_BLOCK) as u64)
    }
}

fn parse_record<'a>(
    geometry: &LogGeometry,
    segment: &Segment,
    buffer: &'a [u8],
    offset: usize,
) -> Option<(RecordType, &'a [u8])> {
    let area = buffer.get(offset..)?;
    if area.len() < RECORD_HEADER {
        return None;
    }
    let kind = RecordType::from_byte(area[8])?;
    if area[9] != 0 {
        return None;
    }
    let length = read_u32(area, 4) as usize;
    let allowed = match kind {
        RecordType::Batch => length == BATCH_PAYLOAD,
        RecordType::Pad => (LOG_BLOCK - offset % LOG_BLOCK)
            .checked_sub(RECORD_HEADER)
            .is_some_and(|room| length <= room),
        RecordType::PageImage => length == IMAGE_PAGE_AT + geometry.page_size.bytes(),
        RecordType::Commit => length == COMMIT_PAYLOAD,
        RecordType::Link => length == LINK_PAYLOAD,
    };
    if !allowed || length > area.len() - RECORD_HEADER {
        return None;
    }
    let end = RECORD_HEADER + length;
    let seed = geometry.record_seed(segment.sequence, geometry.lsn_at(segment, offset));
    if read_u32(area, 0) != record_checksum(seed, &area[4..end]) {
        return None;
    }
    Some((kind, &area[RECORD_HEADER..end]))
}

fn record_checksum(seed: u32, bytes: &[u8]) -> u32 {
    #[cfg(test)]
    tests::CHECKSUMMED_BYTES.with(|total| total.set(total.get() + bytes.len() as u64));
    crc32c_append(seed, bytes)
}

fn align_up(offset: usize) -> usize {
    offset.next_multiple_of(LOG_BLOCK)
}

fn too_close_to_block_end(offset: usize) -> bool {
    let within = offset % LOG_BLOCK;
    within != 0 && LOG_BLOCK - within < RECORD_HEADER
}

/// The fields of the superblock a commit record carries, so that replaying the log restores
/// them without a header update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommitFields {
    pub(crate) stamp: u64,
    pub(crate) page_count: u64,
    pub(crate) free_list_head: u64,
    pub(crate) free_page_count: u64,
    pub(crate) catalog_root: u64,
    pub(crate) clock_high_water: u64,
    pub(crate) features: Features,
}

impl CommitFields {
    pub(crate) fn of(superblock: &Superblock, stamp: u64) -> Self {
        Self {
            stamp,
            page_count: superblock.page_count,
            free_list_head: superblock.free_list_head,
            free_page_count: superblock.free_page_count,
            catalog_root: superblock.catalog_root,
            clock_high_water: superblock.clock_high_water,
            features: superblock.features,
        }
    }

    pub(crate) fn apply(&self, superblock: &mut Superblock) {
        superblock.page_count = self.page_count;
        superblock.free_list_head = self.free_list_head;
        superblock.free_page_count = self.free_page_count;
        superblock.catalog_root = self.catalog_root;
        superblock.clock_high_water = self.clock_high_water;
        superblock.features = self.features;
    }

    fn encode(&self) -> [u8; COMMIT_PAYLOAD] {
        let mut payload = [0u8; COMMIT_PAYLOAD];
        let fields = [
            self.stamp,
            self.page_count,
            self.free_list_head,
            self.free_page_count,
            self.catalog_root,
            self.clock_high_water,
            self.features.compatible,
            self.features.read_only_compatible,
            self.features.incompatible,
        ];
        for (index, field) in fields.into_iter().enumerate() {
            write_u64(&mut payload, 8 * index, field);
        }
        payload
    }

    fn decode(payload: &[u8]) -> Self {
        let field = |index: usize| read_u64(payload, 8 * index);
        Self {
            stamp: field(0),
            page_count: field(1),
            free_list_head: field(2),
            free_page_count: field(3),
            catalog_root: field(4),
            clock_high_water: field(5),
            features: Features {
                compatible: field(6),
                read_only_compatible: field(7),
                incompatible: field(8),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Run {
    file_offset: u64,
    buffer_start: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StagedImage {
    pub(crate) page_id: u64,
    pub(crate) buffer_start: usize,
    pub(crate) file_offset: u64,
}

#[derive(Debug, Clone, Copy)]
struct Mark {
    segment: Segment,
    offset: usize,
    needs_fresh_segment: bool,
    new_segments: usize,
}

/// The end of the log that commits append to: the segment being filled, the bytes of the
/// flush being built, and how far the log is known to be durable.
#[derive(Debug)]
pub(crate) struct LogWriter {
    geometry: LogGeometry,
    segment: Segment,
    offset: usize,
    durable_lsn: u64,
    needs_fresh_segment: bool,
    buffer: Vec<u8>,
    runs: Vec<Run>,
    images: Vec<StagedImage>,
    new_segments: Vec<Segment>,
    mark: Option<Mark>,
}

impl LogWriter {
    pub(crate) fn new(
        geometry: LogGeometry,
        segment: Segment,
        offset: usize,
        needs_fresh_segment: bool,
        durable_lsn: u64,
    ) -> Self {
        Self {
            geometry,
            segment,
            offset,
            durable_lsn,
            needs_fresh_segment,
            buffer: Vec::new(),
            runs: Vec::new(),
            images: Vec::new(),
            new_segments: Vec::new(),
            mark: None,
        }
    }

    pub(crate) fn lsn(&self) -> u64 {
        self.geometry.lsn_at(&self.segment, self.offset)
    }

    pub(crate) fn unsynced_bytes(&self) -> u64 {
        self.lsn().saturating_sub(self.durable_lsn)
    }

    pub(crate) fn images(&self) -> &[StagedImage] {
        &self.images
    }

    pub(crate) fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    pub(crate) fn new_segments(&self) -> &[Segment] {
        &self.new_segments
    }

    pub(crate) fn begin(&mut self, page_count: &mut u64) -> Result<(), StorageError> {
        self.buffer.clear();
        self.runs.clear();
        self.images.clear();
        self.new_segments.clear();
        self.mark = Some(Mark {
            segment: self.segment,
            offset: self.offset,
            needs_fresh_segment: self.needs_fresh_segment,
            new_segments: 0,
        });
        self.start_run();
        if self.needs_fresh_segment {
            self.switch_segment(page_count)?;
            self.needs_fresh_segment = false;
        }
        self.put_batch();
        Ok(())
    }

    pub(crate) fn abort(&mut self) {
        if let Some(mark) = self.mark.take() {
            self.segment = mark.segment;
            self.offset = mark.offset;
            self.needs_fresh_segment = mark.needs_fresh_segment;
            self.new_segments.truncate(mark.new_segments);
        }
        self.buffer.clear();
        self.runs.clear();
        self.images.clear();
    }

    pub(crate) fn reserve(
        &mut self,
        payload: usize,
        page_count: &mut u64,
    ) -> Result<(), StorageError> {
        self.skip_block_tail();
        let end = self.offset + RECORD_HEADER + payload + LINK_RECORD;
        if end > self.geometry.segment_bytes() {
            self.switch_segment(page_count)?;
            self.put_batch();
        }
        Ok(())
    }

    pub(crate) fn put_image(&mut self, page_id: u64, page: &[u8]) {
        let record_start = self.buffer.len();
        let file_offset = self.file_offset_of(record_start) + (RECORD_HEADER + 8) as u64;
        self.put(RecordType::PageImage, &[&page_id.to_le_bytes(), page]);
        self.images.push(StagedImage {
            page_id,
            buffer_start: record_start + RECORD_HEADER + 8,
            file_offset,
        });
    }

    pub(crate) fn put_commit(&mut self, fields: &CommitFields) {
        self.put(RecordType::Commit, &[&fields.encode()]);
    }

    pub(crate) fn seal(&mut self, page_count: &mut u64) -> Result<(), StorageError> {
        let block_end = align_up(self.offset);
        if block_end >= self.geometry.segment_bytes() {
            return self.switch_segment(page_count);
        }
        let gap = block_end - self.offset;
        if gap >= RECORD_HEADER {
            let zeros = [0u8; LOG_BLOCK];
            self.put(RecordType::Pad, &[&zeros[..gap - RECORD_HEADER]]);
        } else {
            self.buffer.resize(self.buffer.len() + gap, 0);
            self.offset = block_end;
        }
        Ok(())
    }

    pub(crate) fn write<F: File>(&mut self, file: &F) -> Result<(), StorageError> {
        for (index, run) in self.runs.iter().enumerate() {
            let end = self
                .runs
                .get(index + 1)
                .map_or(self.buffer.len(), |next| next.buffer_start);
            file.write_all_at(&self.buffer[run.buffer_start..end], run.file_offset)?;
        }
        self.mark = None;
        Ok(())
    }

    pub(crate) fn sync_to<F: File>(&mut self, file: &F, lsn: u64) -> Result<bool, StorageError> {
        if self.durable_lsn >= lsn {
            return Ok(false);
        }
        file.sync_data()?;
        self.durable_lsn = lsn;
        Ok(true)
    }

    fn file_offset_of(&self, buffer_position: usize) -> u64 {
        let run = self
            .runs
            .iter()
            .rev()
            .find(|run| run.buffer_start <= buffer_position)
            .copied()
            .unwrap_or(Run {
                file_offset: 0,
                buffer_start: 0,
            });
        run.file_offset + (buffer_position - run.buffer_start) as u64
    }

    fn start_run(&mut self) {
        let file_offset = self.geometry.file_offset(self.segment.first_page) + self.offset as u64;
        self.runs.push(Run {
            file_offset,
            buffer_start: self.buffer.len(),
        });
    }

    fn skip_block_tail(&mut self) {
        if too_close_to_block_end(self.offset) {
            let block_end = align_up(self.offset);
            self.buffer
                .resize(self.buffer.len() + block_end - self.offset, 0);
            self.offset = block_end;
        }
    }

    fn put_batch(&mut self) {
        self.put(RecordType::Batch, &[&self.durable_lsn.to_le_bytes()]);
    }

    fn switch_segment(&mut self, page_count: &mut u64) -> Result<(), StorageError> {
        let first_page = *page_count;
        let new_page_count = first_page
            .checked_add(u64::from(self.geometry.segment_pages))
            .filter(|&count| count & PAGE_VOLUME_MASK == 0)
            .ok_or(StorageError::FileFull)?;
        let next = Segment {
            first_page,
            sequence: self
                .segment
                .sequence
                .checked_add(1)
                .ok_or(StorageError::FileFull)?,
            start_lsn: self
                .segment
                .start_lsn
                .checked_add(self.geometry.capacity())
                .ok_or(StorageError::FileFull)?,
        };
        self.put(
            RecordType::Link,
            &[&first_page.to_le_bytes(), &new_page_count.to_le_bytes()],
        );
        *page_count = new_page_count;
        self.segment = next;
        self.offset = 0;
        self.start_run();
        let start = self.buffer.len();
        self.buffer.resize(start + LOG_BLOCK, 0);
        self.geometry
            .encode_segment_header(&next, &mut self.buffer[start..]);
        self.offset = LOG_BLOCK;
        self.new_segments.push(next);
        Ok(())
    }

    fn put(&mut self, kind: RecordType, parts: &[&[u8]]) {
        let start = self.buffer.len();
        let length: usize = parts.iter().map(|part| part.len()).sum();
        self.buffer.extend_from_slice(&[0; 4]);
        self.buffer
            .extend_from_slice(&(length as u32).to_le_bytes());
        self.buffer.push(kind.to_byte());
        self.buffer.push(0);
        for part in parts {
            self.buffer.extend_from_slice(part);
        }
        let lsn = self.lsn();
        let seed = self.geometry.record_seed(self.segment.sequence, lsn);
        let checksum = crc32c_append(seed, &self.buffer[start + 4..]);
        write_u32(&mut self.buffer, start, checksum);
        self.offset += RECORD_HEADER + length;
    }
}

/// A committed page image found in the log: the page it belongs to and where its bytes lie
/// in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoggedImage {
    pub(crate) page_id: u64,
    pub(crate) file_offset: u64,
}

/// What reading the log from the checkpoint found.
#[derive(Debug)]
pub(crate) struct Scan {
    pub(crate) segments: Vec<Segment>,
    pub(crate) tail: Segment,
    pub(crate) resume_offset: usize,
    pub(crate) needs_fresh_segment: bool,
    pub(crate) end_lsn: u64,
    pub(crate) commit: Option<CommitFields>,
    pub(crate) commit_at: Option<(Segment, usize)>,
    pub(crate) images: Vec<LoggedImage>,
    pub(crate) linked_page_count: u64,
}

#[derive(Debug, Clone, Copy)]
struct Resume {
    segment: Segment,
    offset: usize,
    segments: usize,
    linked_page_count: u64,
}

fn segment_buffer(geometry: &LogGeometry) -> Result<Vec<u8>, StorageError> {
    let mut buffer = cells::reserved(geometry.segment_bytes())?;
    buffer.resize(geometry.segment_bytes(), 0);
    Ok(buffer)
}

fn read_segment<F: File>(
    file: &F,
    geometry: &LogGeometry,
    first_page: u64,
    buffer: &mut [u8],
) -> Result<bool, StorageError> {
    match file.read_exact_at(buffer, geometry.file_offset(first_page)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn proves_durable_beyond(
    geometry: &LogGeometry,
    segment: &Segment,
    buffer: &[u8],
    offset: usize,
) -> bool {
    let lsn = geometry.lsn_at(segment, offset);
    let batch = RecordType::Batch.to_byte();
    (align_up(offset + 1).max(LOG_BLOCK)..buffer.len())
        .step_by(LOG_BLOCK)
        .filter(|&block| buffer.get(block + 8) == Some(&batch))
        .any(|block| {
            matches!(
                parse_record(geometry, segment, buffer, block),
                Some((RecordType::Batch, payload)) if read_u64(payload, 0) > lsn
            )
        })
}

pub(crate) fn corrupt(segment: &Segment, offset: usize) -> StorageError {
    FormatError::CorruptLog {
        segment: segment.first_page,
        offset: offset as u64,
    }
    .into()
}

fn overlaps(occupied: &BTreeMap<u64, u64>, first: u64, end: u64) -> bool {
    occupied
        .range(..end)
        .next_back()
        .is_some_and(|(_, &other_end)| other_end > first)
}

/// Reads the log of the file whose header is `superblock`, from its checkpoint to its last
/// valid record, verifying every record (format.md, "Write-ahead log").
pub(crate) fn scan<F: File>(
    file: &F,
    geometry: &LogGeometry,
    checksums: &PageChecksum,
    superblock: &Superblock,
) -> Result<Scan, StorageError> {
    let segment_pages = u64::from(geometry.segment_pages);
    let first_data_page = geometry.page_size.first_data_page();
    let mut buffer = segment_buffer(geometry)?;
    let mut next_buffer = segment_buffer(geometry)?;
    let head_page = superblock.log_head_page;
    let head = read_segment(file, geometry, head_page, &mut buffer)?
        .then(|| geometry.decode_segment_header(head_page, &buffer))
        .flatten()
        .filter(|segment| segment.sequence == superblock.log_head_sequence)
        .ok_or(FormatError::InvalidField {
            field: "log_head_sequence",
        })?;
    let checkpoint = superblock
        .checkpoint_lsn
        .checked_sub(head.start_lsn)
        .filter(|&into| into < geometry.capacity() && into.is_multiple_of(LOG_BLOCK as u64))
        .ok_or(FormatError::InvalidField {
            field: "checkpoint_lsn",
        })?;
    let mut segment = head;
    let mut offset = LOG_BLOCK + checkpoint as usize;
    let mut segments = vec![head];
    let mut occupied = BTreeMap::from([(head_page, head_page + segment_pages)]);
    let mut images = Vec::new();
    let mut committed_images = 0;
    let mut commit = None;
    let mut commit_at = None;
    let mut linked_page_count = 0;
    let mut at_commit_boundary = true;
    let mut resume = Resume {
        segment,
        offset,
        segments: 1,
        linked_page_count,
    };
    loop {
        if too_close_to_block_end(offset) {
            offset = align_up(offset);
            if at_commit_boundary {
                resume.offset = offset;
            }
            continue;
        }
        if offset >= buffer.len() {
            break;
        }
        let Some((kind, payload)) = parse_record(geometry, &segment, &buffer, offset) else {
            if proves_durable_beyond(geometry, &segment, &buffer, offset) {
                return Err(corrupt(&segment, offset));
            }
            break;
        };
        let next = offset + RECORD_HEADER + payload.len();
        match kind {
            RecordType::Batch => at_commit_boundary = false,
            RecordType::Pad => {
                if at_commit_boundary {
                    resume.offset = next;
                }
            }
            RecordType::PageImage => {
                let page_id = read_u64(payload, 0);
                if checksums
                    .verify(&payload[IMAGE_PAGE_AT..], page_id)
                    .is_err()
                {
                    return Err(corrupt(&segment, offset));
                }
                let file_offset = geometry.file_offset(segment.first_page)
                    + (offset + RECORD_HEADER + IMAGE_PAGE_AT) as u64;
                images.push(LoggedImage {
                    page_id,
                    file_offset,
                });
                at_commit_boundary = false;
            }
            RecordType::Commit => {
                commit = Some(CommitFields::decode(payload));
                commit_at = Some((segment, offset));
                committed_images = images.len();
                at_commit_boundary = true;
                resume = Resume {
                    segment,
                    offset: next,
                    segments: segments.len(),
                    linked_page_count,
                };
            }
            RecordType::Link => {
                let next_page = read_u64(payload, 0);
                let page_count = read_u64(payload, 8);
                let expected = Segment {
                    first_page: next_page,
                    sequence: segment.sequence.wrapping_add(1),
                    start_lsn: segment.start_lsn.wrapping_add(geometry.capacity()),
                };
                let end = next_page.checked_add(segment_pages);
                let plausible = next_page >= first_data_page
                    && page_count & PAGE_VOLUME_MASK == 0
                    && segment.sequence != u64::MAX
                    && segment.start_lsn.checked_add(geometry.capacity()).is_some()
                    && end.is_some_and(|end| {
                        end == page_count && !overlaps(&occupied, next_page, end)
                    });
                if !plausible {
                    return Err(corrupt(&segment, offset));
                }
                let read = read_segment(file, geometry, next_page, &mut next_buffer)?;
                let found = read
                    .then(|| geometry.decode_segment_header(next_page, &next_buffer))
                    .flatten();
                if found != Some(expected) {
                    if read && proves_durable_beyond(geometry, &expected, &next_buffer, 0) {
                        return Err(corrupt(&expected, 0));
                    }
                    break;
                }
                occupied.insert(next_page, next_page + segment_pages);
                segments.push(expected);
                linked_page_count = linked_page_count.max(page_count);
                std::mem::swap(&mut buffer, &mut next_buffer);
                segment = expected;
                offset = LOG_BLOCK;
                if at_commit_boundary {
                    resume = Resume {
                        segment,
                        offset,
                        segments: segments.len(),
                        linked_page_count,
                    };
                }
                continue;
            }
        }
        offset = next;
    }
    if resume.offset + LINK_RECORD > buffer.len() {
        return Err(corrupt(&resume.segment, resume.offset));
    }
    images.truncate(committed_images);
    segments.truncate(resume.segments);
    let needs_fresh_segment = !resume.offset.is_multiple_of(LOG_BLOCK)
        || resume.segment != segment
        || buffer[resume.offset.min(buffer.len())..]
            .iter()
            .any(|&byte| byte != 0);
    Ok(Scan {
        segments,
        tail: resume.segment,
        resume_offset: resume.offset,
        needs_fresh_segment,
        end_lsn: geometry.lsn_at(&resume.segment, resume.offset),
        commit,
        commit_at,
        images,
        linked_page_count: resume.linked_page_count,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CommitFields, Durability, LOG_BLOCK, LogGeometry, LogWriter, RECORD_HEADER, RecordType,
        Segment, parse_record,
    };
    use crate::error::{FormatError, StorageError};
    use crate::format::{Access, FileId, NO_PAGE, PageHeader, PageSize, PageType, Superblock};
    use crate::page_cache::{CacheSize, PageCache};
    use crate::page_file::{CreateOptions, PageFile};
    use crate::test_rng::TestRng;
    use eggdb_platform::{
        CrashMode, File, FileSystem, OpenMode, SimFile, SimFileSystem, SimMemory,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    thread_local! {
        pub(super) static CHECKSUMMED_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    const DB: &str = "db";
    const FILE_ID: FileId = FileId(*b"write-ahead-log!");

    fn leaf() -> PageHeader {
        PageHeader {
            page_type: PageType::BTreeLeaf,
            layout: 1,
            flags: 0,
            lsn: 0,
        }
    }

    fn options(page_size: PageSize) -> CreateOptions {
        CreateOptions {
            page_size,
            file_id: FILE_ID,
        }
    }

    fn create(fs: &SimFileSystem) -> PageCache<SimFile> {
        let file = PageFile::create(fs, Path::new(DB), options(PageSize::DEFAULT)).unwrap();
        PageCache::new(file, &SimMemory::new(), CacheSize::Pages(64)).unwrap()
    }

    fn open(fs: &SimFileSystem, access: Access) -> Result<PageCache<SimFile>, StorageError> {
        let file = PageFile::open(fs, Path::new(DB), access)?;
        PageCache::new(file, &SimMemory::new(), CacheSize::Pages(64))
    }

    fn fill_of(cache: &PageCache<SimFile>, page: u64) -> Result<u8, StorageError> {
        let bytes = cache.read(page)?;
        assert!(bytes.iter().all(|&byte| byte == bytes[0]), "page {page}");
        Ok(bytes[0])
    }

    fn set_fill(cache: &PageCache<SimFile>, page: u64, fill: u8) {
        cache.write(page).unwrap().fill(fill);
    }

    fn allocate(cache: &PageCache<SimFile>, fill: u8) -> u64 {
        let mut page = cache.allocate(leaf()).unwrap();
        page.fill(fill);
        page.page_id()
    }

    fn raw(fs: &SimFileSystem) -> SimFile {
        fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap()
    }

    fn geometry() -> LogGeometry {
        LogGeometry::new(FILE_ID, PageSize::DEFAULT, 16)
    }

    fn segment() -> Segment {
        Segment {
            first_page: 2,
            sequence: 7,
            start_lsn: 1 << 20,
        }
    }

    fn segment_with(writer: &LogWriter) -> Vec<u8> {
        let mut bytes = vec![0u8; 16 * LOG_BLOCK];
        let flush = writer.buffer();
        bytes[LOG_BLOCK..LOG_BLOCK + flush.len()].copy_from_slice(flush);
        bytes
    }

    fn records(writer: &LogWriter) -> Vec<(RecordType, Vec<u8>)> {
        let bytes = segment_with(writer);
        let end = LOG_BLOCK + writer.buffer().len();
        let mut found = Vec::new();
        let mut at = LOG_BLOCK;
        while at < end {
            if super::too_close_to_block_end(at) {
                at = super::align_up(at);
                continue;
            }
            let Some((kind, payload)) = parse_record(&geometry(), &segment(), &bytes, at) else {
                break;
            };
            found.push((kind, payload.to_vec()));
            at += RECORD_HEADER + payload.len();
        }
        found
    }

    fn sample_flush() -> (LogWriter, Vec<u8>) {
        let mut writer = LogWriter::new(geometry(), segment(), LOG_BLOCK, false, 1 << 20);
        let mut page_count = 18;
        writer.begin(&mut page_count).unwrap();
        let mut page = vec![0x5A; 4096];
        crate::format::PageChecksum::new(FILE_ID, PageSize::DEFAULT)
            .write(&mut page, 30, &leaf())
            .unwrap();
        writer.reserve(8 + 4096, &mut page_count).unwrap();
        writer.put_image(30, &page);
        let fields = CommitFields {
            stamp: 9,
            page_count: 40,
            free_list_head: NO_PAGE,
            free_page_count: 0,
            catalog_root: 30,
            clock_high_water: 3,
            features: Default::default(),
        };
        writer
            .reserve(super::COMMIT_PAYLOAD, &mut page_count)
            .unwrap();
        writer.put_commit(&fields);
        writer.seal(&mut page_count).unwrap();
        (writer, page)
    }

    #[test]
    fn records_survive_encoding_and_a_flush_ends_on_a_block_boundary() {
        let (writer, page) = sample_flush();
        assert!(writer.buffer().len().is_multiple_of(LOG_BLOCK));
        let found = records(&writer);
        let kinds: Vec<RecordType> = found.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            [
                RecordType::Batch,
                RecordType::PageImage,
                RecordType::Commit,
                RecordType::Pad
            ]
        );
        assert_eq!(found[0].1, (1u64 << 20).to_le_bytes());
        assert_eq!(found[1].1[..8], 30u64.to_le_bytes());
        assert_eq!(found[1].1[8..], page[..]);
        let commit = CommitFields::decode(&found[2].1);
        assert_eq!(
            (commit.stamp, commit.page_count, commit.catalog_root),
            (9, 40, 30)
        );
        assert_eq!(writer.images()[0].page_id, 30);
        assert_eq!(writer.lsn(), (1 << 20) + writer.buffer().len() as u64);
    }

    #[test]
    fn a_record_fails_for_any_flipped_bit_another_place_sequence_or_file() {
        let (writer, _) = sample_flush();
        let bytes = segment_with(&writer);
        let image_at = LOG_BLOCK + RECORD_HEADER + 8;
        let image_len = RECORD_HEADER + 8 + 4096;
        assert!(parse_record(&geometry(), &segment(), &bytes, image_at).is_some());
        let mut rng = TestRng::new(0x5EC);
        for _ in 0..2000 {
            let mut damaged = bytes.clone();
            let bit = rng.below(image_len * 8);
            damaged[image_at + bit / 8] ^= 1 << (bit % 8);
            let parsed = parse_record(&geometry(), &segment(), &damaged, image_at);
            assert!(parsed.is_none(), "bit {bit}");
        }
        let mut moved = vec![0u8; bytes.len()];
        let moved_at = image_at + 3 * LOG_BLOCK;
        moved[moved_at..moved_at + image_len]
            .copy_from_slice(&bytes[image_at..image_at + image_len]);
        assert!(parse_record(&geometry(), &segment(), &moved, moved_at).is_none());
        let next_sequence = Segment {
            sequence: 8,
            ..segment()
        };
        assert!(parse_record(&geometry(), &next_sequence, &bytes, image_at).is_none());
        let other_file = LogGeometry::new(FileId(*b"another-file-id!"), PageSize::DEFAULT, 16);
        assert!(parse_record(&other_file, &segment(), &bytes, image_at).is_none());
    }

    #[test]
    fn a_record_whose_length_does_not_fit_its_type_is_refused_before_its_checksum() {
        let geometry = LogGeometry::new(FILE_ID, PageSize::from_log2(16).unwrap(), 1024);
        let head = Segment {
            first_page: 1,
            sequence: 1,
            start_lsn: 0,
        };
        let mut bytes = vec![0u8; 1024 << 16];
        for block in (LOG_BLOCK..bytes.len()).step_by(LOG_BLOCK) {
            let rest = (bytes.len() - block - RECORD_HEADER) as u32;
            bytes[block + 4..block + 8].copy_from_slice(&rest.to_le_bytes());
            bytes[block + 8] = RecordType::Batch.to_byte();
        }
        CHECKSUMMED_BYTES.with(|total| total.set(0));
        assert!(!super::proves_durable_beyond(
            &geometry, &head, &bytes, LOG_BLOCK
        ));
        for block in (LOG_BLOCK..bytes.len()).step_by(LOG_BLOCK) {
            assert!(parse_record(&geometry, &head, &bytes, block).is_none());
        }
        assert_eq!(CHECKSUMMED_BYTES.with(|total| total.get()), 0);
    }

    #[test]
    fn a_segment_header_names_its_page_and_sequence() {
        let geometry = geometry();
        let mut block = vec![0u8; LOG_BLOCK];
        geometry.encode_segment_header(&segment(), &mut block);
        assert_eq!(geometry.decode_segment_header(2, &block), Some(segment()));
        assert_eq!(geometry.decode_segment_header(3, &block), None);
        block[20] ^= 1;
        assert_eq!(geometry.decode_segment_header(2, &block), None);
    }

    #[test]
    fn segments_must_hold_a_whole_page_image() {
        for log2 in 12..=16 {
            let page_size = PageSize::from_log2(log2).unwrap();
            let default = super::default_segment_pages(page_size);
            assert!(super::segment_holds_a_page(page_size, default));
            let bytes = u64::from(default) * page_size.bytes() as u64;
            assert!(bytes >= 64 << 10);
        }
        assert!(!super::segment_holds_a_page(PageSize::DEFAULT, 2));
        assert!(super::segment_holds_a_page(PageSize::DEFAULT, 3));
        let large = PageSize::from_log2(16).unwrap();
        assert!(!super::segment_holds_a_page(large, 1));
    }

    fn committed_states(fs: &SimFileSystem, durability: Durability) -> Vec<u64> {
        let mut cache = create(fs);
        cache.set_durability(durability).unwrap();
        let pages: Vec<u64> = (0..4).map(|_| allocate(&cache, 1)).collect();
        cache
            .commit(|superblock| superblock.clock_high_water = 1)
            .unwrap();
        for round in 2..=4u8 {
            for &page in &pages {
                set_fill(&cache, page, round);
            }
            cache
                .commit(|superblock| superblock.clock_high_water = u64::from(round))
                .unwrap();
        }
        pages
    }

    #[test]
    fn full_syncs_once_per_commit_and_normal_shares_one_sync_between_commits() {
        let syncs_for = |durability| {
            let fs = SimFileSystem::new();
            let mut cache = create(&fs);
            cache.commit(|_| {}).unwrap();
            cache.set_durability(durability).unwrap();
            let page = allocate(&cache, 1);
            cache.commit(|_| {}).unwrap();
            let before = fs.sync_count();
            for fill in 2..5 {
                set_fill(&cache, page, fill);
                cache.commit(|_| {}).unwrap();
            }
            let commits = fs.sync_count() - before;
            cache.sync().unwrap();
            let group = fs.sync_count() - before - commits;
            cache.sync().unwrap();
            let again = fs.sync_count() - before - commits - group;
            (commits, group, again)
        };
        assert_eq!(syncs_for(Durability::Full), (3, 0, 0));
        assert_eq!(syncs_for(Durability::Normal), (0, 1, 0));
        assert_eq!(syncs_for(Durability::Off), (0, 1, 0));
    }

    #[test]
    fn a_group_synced_once_survives_a_power_loss_whole() {
        let fs = SimFileSystem::new();
        let pages = committed_states(&fs, Durability::Normal);
        let mut cache = open(&fs, Access::ReadWrite).unwrap();
        cache.set_durability(Durability::Normal).unwrap();
        for fill in [5, 6, 7] {
            set_fill(&cache, pages[0], fill);
            cache
                .commit(|superblock| superblock.clock_high_water = u64::from(fill))
                .unwrap();
        }
        let before = fs.sync_count();
        cache.sync().unwrap();
        assert_eq!(fs.sync_count() - before, 1);
        drop(cache);
        fs.crash(CrashMode::Torn { seed: 3 });
        let cache = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(cache.superblock().clock_high_water, 7);
        assert_eq!(fill_of(&cache, pages[0]).unwrap(), 7);
    }

    #[test]
    fn a_power_loss_under_normal_loses_only_the_latest_commits() {
        for seed in 0..32 {
            let fs = SimFileSystem::new();
            let pages = committed_states(&fs, Durability::Normal);
            fs.crash(CrashMode::Torn { seed });
            let cache = open(&fs, Access::ReadWrite).unwrap();
            let round = cache.superblock().clock_high_water;
            if round == 0 {
                continue;
            }
            for &page in &pages {
                assert_eq!(
                    u64::from(fill_of(&cache, page).unwrap()),
                    round,
                    "seed {seed}"
                );
            }
        }
    }

    fn small_commits(fs: &SimFileSystem) -> u64 {
        let mut cache = create(fs);
        let page = allocate(&cache, 1);
        cache
            .commit(|superblock| superblock.clock_high_water = 1)
            .unwrap();
        for round in 2..=4u8 {
            set_fill(&cache, page, round);
            cache
                .commit(|superblock| superblock.clock_high_water = u64::from(round))
                .unwrap();
        }
        page
    }

    #[test]
    fn a_damaged_record_with_durable_records_after_it_is_corruption_by_position() {
        let fs = SimFileSystem::new();
        small_commits(&fs);
        let cache = open(&fs, Access::ReadOnly).unwrap();
        let head = cache.superblock().log_head_page;
        drop(cache);
        let damaged_at = 2 * LOG_BLOCK as u64 + 100;
        let raw = raw(&fs);
        let mut byte = [0u8];
        raw.read_exact_at(&mut byte, head * 4096 + damaged_at)
            .unwrap();
        raw.write_all_at(&[byte[0] ^ 0x40], head * 4096 + damaged_at)
            .unwrap();
        drop(raw);
        let error = open(&fs, Access::ReadOnly).unwrap_err();
        assert!(
            matches!(error, StorageError::Format(FormatError::CorruptLog { segment, offset }) if segment == head && offset <= damaged_at && offset >= LOG_BLOCK as u64),
            "{error}"
        );
        assert!(error.to_string().contains(&format!("page {head}")));
    }

    #[test]
    fn a_damaged_last_flush_is_a_torn_tail_and_opens_the_commit_before_it() {
        let fs = SimFileSystem::new();
        let page = small_commits(&fs);
        let cache = open(&fs, Access::ReadOnly).unwrap();
        let head = cache.superblock().log_head_page;
        drop(cache);
        let raw = raw(&fs);
        let mut last_batch = 0;
        let mut bytes = vec![0u8; 16 * LOG_BLOCK];
        raw.read_exact_at(&mut bytes, head * 4096).unwrap();
        let head_segment = Segment {
            first_page: head,
            sequence: 1,
            start_lsn: 0,
        };
        for index in 1..16u64 {
            let at = index as usize * LOG_BLOCK;
            if matches!(
                parse_record(&geometry(), &head_segment, &bytes, at),
                Some((RecordType::Batch, _))
            ) {
                last_batch = index;
            }
        }
        raw.write_all_at(&[0xFF; 64], (head + last_batch) * 4096 + 600)
            .unwrap();
        drop(raw);
        let cache = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(cache.superblock().clock_high_water, 3);
        assert_eq!(fill_of(&cache, page).unwrap(), 3);
    }

    #[test]
    fn a_log_head_without_its_sequence_refuses_the_file() {
        let fs = SimFileSystem::new();
        committed_states(&fs, Durability::Full);
        let mut header = {
            let cache = open(&fs, Access::ReadOnly).unwrap();
            let file = PageFile::open(&fs, Path::new("other"), Access::ReadOnly);
            assert!(file.is_err());
            cache.superblock()
        };
        let raw = raw(&fs);
        let mut slots = vec![0u8; 8192];
        raw.read_exact_at(&mut slots, 0).unwrap();
        let current = Superblock::choose(&slots[..4096], &slots[4096..]).unwrap();
        header.generation = current.superblock.generation + 1;
        header.page_count = current.superblock.page_count;
        header.free_list_head = current.superblock.free_list_head;
        header.free_page_count = current.superblock.free_page_count;
        header.log_head_sequence += 1;
        raw.write_all_at(&header.encode().unwrap(), (header.slot() * 4096) as u64)
            .unwrap();
        drop(raw);
        assert!(matches!(
            open(&fs, Access::ReadOnly),
            Err(StorageError::Format(FormatError::InvalidField {
                field: "log_head_sequence"
            }))
        ));
    }

    fn first_commits(fs: &SimFileSystem) -> Vec<u64> {
        let mut cache = create(fs);
        let pages: Vec<u64> = (0..10).map(|_| allocate(&cache, 1)).collect();
        cache
            .commit(|superblock| superblock.catalog_root = pages[0])
            .unwrap();
        for &page in &pages[..5] {
            set_fill(&cache, page, 2);
        }
        cache.commit(|_| {}).unwrap();
        pages
    }

    fn third_commit(fs: &SimFileSystem, pages: &[u64], fail_at: Option<u64>) -> u64 {
        let mut cache = open(fs, Access::ReadWrite).unwrap();
        for &page in &pages[3..8] {
            set_fill(&cache, page, 3);
        }
        cache.free(pages[9]).unwrap();
        let added: Vec<u64> = (0..2).map(|_| allocate(&cache, 3)).collect();
        let start = fs.operation_count();
        if let Some(offset) = fail_at {
            fs.fail_operation(start + offset);
        }
        let committed = cache.commit(|superblock| superblock.catalog_root = added[0]);
        assert_eq!(committed.is_ok(), fail_at.is_none());
        fs.operation_count() - start
    }

    fn expected_fills(pages: &[u64], root: u64) -> BTreeMap<u64, Option<u8>> {
        let third = root != pages[0];
        let mut fills: BTreeMap<u64, Option<u8>> = pages
            .iter()
            .enumerate()
            .map(|(index, &page)| {
                let fill = match index {
                    0..3 => 2,
                    3..5 if third => 3,
                    3..5 => 2,
                    5..8 if third => 3,
                    _ => 1,
                };
                (page, Some(fill))
            })
            .collect();
        if third {
            fills.insert(pages[9], None);
            fills.insert(root, Some(3));
            fills.insert(root + 1, Some(3));
        }
        fills
    }

    fn check_state(cache: &PageCache<SimFile>, pages: &[u64], context: &str) -> bool {
        let root = cache.superblock().catalog_root;
        for (page, fill) in expected_fills(pages, root) {
            match fill {
                Some(fill) => assert_eq!(fill_of(cache, page).unwrap(), fill, "{context}"),
                None => assert!(cache.read(page).is_err(), "{context}"),
            }
        }
        root != pages[0]
    }

    #[test]
    fn a_crash_at_any_operation_of_a_commit_that_overwrites_pages_keeps_the_last_durable_commit() {
        let operations = {
            let fs = SimFileSystem::new();
            let pages = first_commits(&fs);
            let operations = third_commit(&fs, &pages, None);
            fs.crash(CrashMode::Torn { seed: 1 });
            let cache = open(&fs, Access::ReadOnly).unwrap();
            assert!(check_state(&cache, &pages, "durable"));
            operations
        };
        let mut outcomes = BTreeSet::new();
        for step in 0..operations {
            for seed in 0..4 {
                let context = format!("step {step}, seed {seed}");
                let fs = SimFileSystem::new();
                let pages = first_commits(&fs);
                third_commit(&fs, &pages, Some(step));
                fs.crash(CrashMode::Torn { seed });
                let mut cache = open(&fs, Access::ReadWrite).unwrap();
                outcomes.insert(check_state(&cache, &pages, &context));
                let root = cache.superblock().catalog_root;
                set_fill(&cache, pages[2], 9);
                cache.commit(|_| {}).unwrap();
                drop(cache);
                fs.crash(CrashMode::Torn { seed: seed + 100 });
                let cache = open(&fs, Access::ReadOnly).unwrap();
                assert_eq!(cache.superblock().catalog_root, root, "{context}");
                assert_eq!(fill_of(&cache, pages[2]).unwrap(), 9, "{context}");
                assert_eq!(
                    fill_of(&cache, pages[4]).unwrap(),
                    if root == pages[0] { 2 } else { 3 }
                );
            }
        }
        assert_eq!(outcomes, BTreeSet::from([false, true]));
    }

    #[test]
    fn commits_that_span_and_exactly_fill_segments_reopen_after_each_one() {
        for log2 in [12, 16] {
            let fs = SimFileSystem::new();
            let page_size = PageSize::from_log2(log2).unwrap();
            let file = PageFile::create(&fs, Path::new(DB), options(page_size)).unwrap();
            let mut cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(48)).unwrap();
            let pages: Vec<u64> = (0..40).map(|_| allocate(&cache, 0)).collect();
            cache.commit(|_| {}).unwrap();
            let mut model: BTreeMap<u64, u8> = pages.iter().map(|&page| (page, 0)).collect();
            let mut rng = TestRng::new(u64::from(log2));
            for round in 1..=30u8 {
                let touched = 1 + rng.below(if round % 10 == 0 { 40 } else { 3 });
                for _ in 0..touched {
                    let page = pages[rng.below(pages.len())];
                    set_fill(&cache, page, round);
                    model.insert(page, round);
                }
                cache.commit(|_| {}).unwrap();
                drop(cache);
                fs.crash(CrashMode::Torn {
                    seed: u64::from(round),
                });
                let file = PageFile::open(&fs, Path::new(DB), Access::ReadWrite).unwrap();
                cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(48)).unwrap();
                for (&page, &fill) in &model {
                    assert_eq!(
                        fill_of(&cache, page).unwrap(),
                        fill,
                        "page {page}, round {round}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_file_without_a_log_opens_and_gets_one_at_its_first_change() {
        let fs = SimFileSystem::new();
        let page = {
            let mut file =
                PageFile::create(&fs, Path::new(DB), options(PageSize::DEFAULT)).unwrap();
            let raw = raw(&fs);
            let mut bytes = vec![7u8; 4096];
            let checksums = crate::format::PageChecksum::new(FILE_ID, PageSize::DEFAULT);
            checksums.write(&mut bytes, 2, &leaf()).unwrap();
            raw.write_all_at(&bytes, 2 * 4096).unwrap();
            let mut legacy = *file.superblock();
            legacy.generation = 1;
            legacy.page_count = 3;
            legacy.catalog_root = 2;
            legacy.log_segment_pages = 1;
            raw.write_all_at(&legacy.encode().unwrap(), 4096).unwrap();
            raw.sync_all().unwrap();
            assert!(file.sync().is_ok());
            2
        };
        let before = fs.operation_count();
        let cache = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(cache.superblock().log_head_page, NO_PAGE);
        assert_eq!(fill_of(&cache, page).unwrap(), 7);
        drop(cache);
        let reads_only = fs.operation_count() - before;
        assert!(reads_only < 10);
        let raw_size = raw(&fs).size().unwrap();
        assert_eq!(raw_size, 3 * 4096);
        let mut cache = open(&fs, Access::ReadWrite).unwrap();
        set_fill(&cache, page, 8);
        cache.commit(|_| {}).unwrap();
        assert_eq!(cache.superblock().log_head_page, 3);
        assert_eq!(cache.superblock().log_segment_pages, 16);
        drop(cache);
        fs.crash(CrashMode::Torn { seed: 5 });
        let cache = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(fill_of(&cache, page).unwrap(), 8);
        assert_eq!(cache.superblock().catalog_root, 2);
    }

    #[test]
    fn an_empty_log_reopens_and_takes_commits() {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs);
        let page = allocate(&cache, 4);
        cache.rollback().unwrap();
        drop(cache);
        fs.crash(CrashMode::Torn { seed: 9 });
        let mut cache = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(cache.superblock().log_head_page, 2);
        assert_eq!(allocate(&cache, 5), page);
        cache.commit(|_| {}).unwrap();
        drop(cache);
        fs.crash(CrashMode::Torn { seed: 10 });
        let cache = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(fill_of(&cache, page).unwrap(), 5);
    }

    #[test]
    fn a_crash_at_any_operation_while_the_log_is_created_leaves_a_usable_file() {
        let operations_for = |fail_at: Option<u64>| {
            let fs = SimFileSystem::new();
            let cache = create(&fs);
            let start = fs.operation_count();
            if let Some(offset) = fail_at {
                fs.fail_operation(start + offset);
            }
            let allocated = cache.allocate(leaf()).map(|page| page.page_id());
            assert_eq!(allocated.is_ok(), fail_at.is_none());
            (fs, start)
        };
        let (fs, start) = operations_for(None);
        let operations = fs.operation_count() - start;
        for step in 0..operations {
            for seed in 0..4 {
                let (fs, _) = operations_for(Some(step));
                fs.crash(CrashMode::Torn { seed });
                let mut cache = open(&fs, Access::ReadWrite).unwrap();
                let page = allocate(&cache, 6);
                cache.commit(|_| {}).unwrap();
                drop(cache);
                fs.crash(CrashMode::Torn { seed: seed + 50 });
                let cache = open(&fs, Access::ReadOnly).unwrap();
                assert_eq!(
                    fill_of(&cache, page).unwrap(),
                    6,
                    "step {step}, seed {seed}"
                );
            }
        }
    }

    #[test]
    fn pages_committed_under_normal_are_read_back_from_the_log_after_eviction() {
        let fs = SimFileSystem::new();
        let file = PageFile::create(&fs, Path::new(DB), options(PageSize::DEFAULT)).unwrap();
        let mut cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(2)).unwrap();
        cache.set_durability(Durability::Normal).unwrap();
        let pages: Vec<u64> = (0..6)
            .map(|fill| {
                let page = allocate(&cache, fill);
                cache.commit(|_| {}).unwrap();
                page
            })
            .collect();
        assert!(cache.stats().evictions > 0);
        for (fill, &page) in pages.iter().enumerate() {
            assert_eq!(fill_of(&cache, page).unwrap(), fill as u8);
        }
        let reader = PageFile::open(&fs, Path::new(DB), Access::ReadOnly);
        assert!(matches!(reader, Err(StorageError::Locked)));
        drop(cache);
        let cache = open(&fs, Access::ReadOnly).unwrap();
        for (fill, &page) in pages.iter().enumerate() {
            assert_eq!(fill_of(&cache, page).unwrap(), fill as u8);
        }
    }

    #[test]
    fn image_less_commits_then_a_reopen_then_a_power_loss_never_refuse_the_file() {
        for seed in 0..64 {
            let fs = SimFileSystem::new();
            let mut cache = create(&fs);
            cache.commit(|_| {}).unwrap();
            cache.set_durability(Durability::Normal).unwrap();
            for round in 1..=3 {
                cache
                    .commit(|superblock| superblock.clock_high_water = round)
                    .unwrap();
            }
            drop(cache);
            let mut cache = open(&fs, Access::ReadWrite).unwrap();
            assert_eq!(cache.superblock().clock_high_water, 3);
            cache.set_durability(Durability::Normal).unwrap();
            allocate(&cache, 7);
            cache
                .commit(|superblock| superblock.clock_high_water = 4)
                .unwrap();
            drop(cache);
            fs.crash(CrashMode::Torn { seed });
            let cache = open(&fs, Access::ReadOnly).unwrap();
            assert!(cache.superblock().clock_high_water >= 3, "seed {seed}");
        }
    }

    #[test]
    fn a_full_commit_cut_at_any_operation_after_image_less_commits_never_refuses_the_file() {
        for step in 0..8 {
            for seed in 0..16 {
                let fs = SimFileSystem::new();
                let mut cache = create(&fs);
                cache.commit(|_| {}).unwrap();
                cache.set_durability(Durability::Normal).unwrap();
                cache
                    .commit(|superblock| superblock.clock_high_water = 1)
                    .unwrap();
                drop(cache);
                let mut cache = open(&fs, Access::ReadWrite).unwrap();
                allocate(&cache, 3);
                let failing = fs.operation_count() + step;
                fs.fail_operation(failing);
                let _ = cache.commit(|superblock| superblock.clock_high_water = 2);
                drop(cache);
                while fs.operation_count() <= failing {
                    let _ = fs.exists(Path::new(DB));
                }
                fs.crash(CrashMode::Torn { seed });
                let cache = open(&fs, Access::ReadOnly).unwrap();
                assert!(
                    cache.superblock().clock_high_water >= 1,
                    "step {step}, seed {seed}"
                );
            }
        }
    }

    fn rounds_with_images(fs: &SimFileSystem, durability: Durability) -> Vec<u64> {
        let mut cache = create(fs);
        let pages: Vec<u64> = (0..4).map(|_| allocate(&cache, 1)).collect();
        cache
            .commit(|superblock| superblock.clock_high_water = 1)
            .unwrap();
        cache.set_durability(durability).unwrap();
        for round in 2..=4u8 {
            for &page in &pages {
                set_fill(&cache, page, round);
            }
            cache
                .commit(|superblock| superblock.clock_high_water = u64::from(round))
                .unwrap();
        }
        pages
    }

    #[test]
    fn a_crash_at_any_operation_of_replaying_the_log_keeps_the_replayed_state() {
        for durability in [Durability::Full, Durability::Normal] {
            let operations = {
                let fs = SimFileSystem::new();
                rounds_with_images(&fs, durability);
                let before = fs.operation_count();
                drop(open(&fs, Access::ReadWrite).unwrap());
                fs.operation_count() - before
            };
            for step in 0..operations {
                for seed in 0..4 {
                    let fs = SimFileSystem::new();
                    let pages = rounds_with_images(&fs, durability);
                    fs.fail_operation(fs.operation_count() + step);
                    assert!(open(&fs, Access::ReadWrite).is_err());
                    fs.crash(CrashMode::Torn { seed });
                    let cache = open(&fs, Access::ReadWrite).unwrap();
                    let round = cache.superblock().clock_high_water;
                    if durability == Durability::Full {
                        assert_eq!(round, 4, "step {step}, seed {seed}");
                    }
                    for &page in &pages {
                        let fill = u64::from(fill_of(&cache, page).unwrap());
                        assert_eq!(fill, round, "step {step}, seed {seed}");
                    }
                }
            }
        }
    }

    fn forged_commit(
        fs: &SimFileSystem,
        fields: impl FnOnce(&mut CommitFields),
        image: Option<u64>,
    ) {
        let mut cache = create(fs);
        let page = allocate(&cache, 1);
        cache.commit(|_| {}).unwrap();
        let superblock = cache.superblock();
        drop(cache);
        let geometry = LogGeometry::new(FILE_ID, PageSize::DEFAULT, superblock.log_segment_pages);
        let raw = raw(fs);
        let head = Segment {
            first_page: superblock.log_head_page,
            sequence: superblock.log_head_sequence,
            start_lsn: superblock.checkpoint_lsn,
        };
        let mut writer = LogWriter::new(geometry, head, LOG_BLOCK, false, 0);
        let mut page_count = superblock.page_count;
        writer.begin(&mut page_count).unwrap();
        if let Some(target) = image.map(|_| page) {
            let mut bytes = vec![0xAB; 4096];
            crate::format::PageChecksum::new(FILE_ID, PageSize::DEFAULT)
                .write(&mut bytes, target, &leaf())
                .unwrap();
            writer.reserve(8 + 4096, &mut page_count).unwrap();
            writer.put_image(target, &bytes);
        }
        let mut commit = CommitFields::of(&superblock, 0);
        fields(&mut commit);
        writer
            .reserve(super::COMMIT_PAYLOAD, &mut page_count)
            .unwrap();
        writer.put_commit(&commit);
        writer.seal(&mut page_count).unwrap();
        writer.write(&raw).unwrap();
        raw.sync_all().unwrap();
    }

    fn file_bytes(fs: &SimFileSystem) -> Vec<u8> {
        let raw = raw(fs);
        let mut bytes = vec![0u8; raw.size().unwrap() as usize];
        raw.read_exact_at(&mut bytes, 0).unwrap();
        bytes
    }

    #[test]
    fn a_replayed_page_count_the_file_can_not_prove_refuses_the_open_without_growing_it() {
        for page_count in [1 << 40, (1 << 48) - 1, 40] {
            let fs = SimFileSystem::new();
            forged_commit(&fs, |commit| commit.page_count = page_count, None);
            let before = file_bytes(&fs);
            for access in [Access::ReadWrite, Access::ReadOnly] {
                let error = open(&fs, access).unwrap_err();
                assert!(
                    matches!(error, StorageError::Format(FormatError::CorruptLog { .. })),
                    "{page_count}: {error}"
                );
            }
            assert_eq!(file_bytes(&fs), before);
        }
        let fs = SimFileSystem::new();
        forged_commit(&fs, |commit| commit.page_count = 19, Some(0));
        assert_eq!(
            open(&fs, Access::ReadWrite)
                .unwrap()
                .superblock()
                .page_count,
            19
        );
    }

    #[test]
    fn unknown_features_in_a_replayed_commit_refuse_the_open_before_any_write() {
        let fs = SimFileSystem::new();
        forged_commit(
            &fs,
            |commit| commit.features.incompatible = 1 << 63,
            Some(0),
        );
        let before = file_bytes(&fs);
        let syncs = fs.sync_count();
        assert!(matches!(
            open(&fs, Access::ReadWrite),
            Err(StorageError::Format(FormatError::UnsupportedFeatures { bits })) if bits == 1 << 63
        ));
        assert_eq!(file_bytes(&fs), before);
        assert_eq!(fs.sync_count(), syncs);
        let fs = SimFileSystem::new();
        forged_commit(
            &fs,
            |commit| commit.features.read_only_compatible = 1 << 2,
            Some(0),
        );
        let before = file_bytes(&fs);
        assert!(matches!(
            open(&fs, Access::ReadWrite),
            Err(StorageError::Format(FormatError::ReadOnlyFeatures { .. }))
        ));
        assert_eq!(file_bytes(&fs), before);
        let reader = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(fill_of(&reader, 18).unwrap(), 0xAB);
    }

    fn normal_rounds_until_a_sync(fs: &SimFileSystem, fail_at: Option<(u8, u64)>) -> (u8, u64) {
        let mut file = PageFile::create(fs, Path::new(DB), options(PageSize::DEFAULT)).unwrap();
        let page = file.allocate().unwrap();
        let mut bytes = vec![0u8; 4096];
        file.write_page(page, &mut bytes, &leaf()).unwrap();
        file.commit(|_| {}).unwrap();
        file.set_durability(Durability::Normal).unwrap();
        for round in 1..=250u8 {
            let mut bytes = vec![round; 4096];
            file.write_page(page, &mut bytes, &leaf()).unwrap();
            if let Some((failing_round, offset)) = fail_at
                && failing_round == round
            {
                fs.fail_operation(fs.operation_count() + offset);
                assert!(file.commit(|_| {}).is_err());
                file.read_page(page, &mut bytes).unwrap();
                return (round, u64::from(bytes[100]));
            }
            let syncs = fs.sync_count();
            file.commit(|_| {}).unwrap();
            if fs.sync_count() > syncs {
                return (round, page);
            }
        }
        panic!("no group sync in 250 commits");
    }

    #[test]
    fn a_failed_group_sync_under_normal_never_serves_the_failed_commit() {
        let (sync_round, _) = normal_rounds_until_a_sync(&SimFileSystem::new(), None);
        assert!(sync_round > 100);
        for offset in [1, 2] {
            let fs = SimFileSystem::new();
            let (round, fill) = normal_rounds_until_a_sync(&fs, Some((sync_round, offset)));
            assert_eq!(round, sync_round);
            assert_eq!(fill, u64::from(sync_round - 1), "offset {offset}");
        }
    }

    #[test]
    fn pages_allocated_but_never_written_under_normal_survive_a_power_loss() {
        for seed in 0..16 {
            let fs = SimFileSystem::new();
            let mut file =
                PageFile::create(&fs, Path::new(DB), options(PageSize::DEFAULT)).unwrap();
            file.commit(|_| {}).unwrap();
            file.set_durability(Durability::Normal).unwrap();
            for _ in 0..5 {
                file.allocate().unwrap();
            }
            file.commit(|superblock| superblock.clock_high_water = 1)
                .unwrap();
            drop(file);
            fs.crash(CrashMode::Torn { seed });
            let file = PageFile::open(&fs, Path::new(DB), Access::ReadWrite).unwrap();
            let count = file.superblock().page_count;
            assert!(count == 18 || count == 23, "seed {seed}: {count}");
        }
    }

    fn log_image(fs: &SimFileSystem) -> (Vec<u8>, u64) {
        committed_states(fs, Durability::Full);
        let head = open(fs, Access::ReadOnly)
            .unwrap()
            .superblock()
            .log_head_page;
        let raw = raw(fs);
        let mut bytes = vec![0u8; raw.size().unwrap() as usize];
        raw.read_exact_at(&mut bytes, 0).unwrap();
        (bytes, head)
    }

    #[test]
    fn mutated_and_random_logs_never_panic_or_hang() {
        let (image, head) = log_image(&SimFileSystem::new());
        let log_start = head as usize * 4096;
        let log_end = log_start + 16 * 4096;
        let mut rng = TestRng::new(0x0F02_2106);
        let geometry = geometry();
        let fs = SimFileSystem::new();
        let raw = fs.open(Path::new(DB), OpenMode::CreateNew).unwrap();
        let mut opened = 0;
        for case in 0..400 {
            let mut bytes = image.clone();
            match case % 4 {
                0 => {
                    for _ in 0..1 + rng.below(8) {
                        let at = log_start + rng.below(log_end - log_start);
                        bytes[at] = rng.next_u64() as u8;
                    }
                }
                1 => {
                    let block = log_start + LOG_BLOCK * (1 + rng.below(15));
                    let kind = 1 + rng.below(5) as u8;
                    let length = match kind {
                        1 => 8,
                        2 => rng.below(64),
                        3 => 8 + 4096,
                        4 => super::COMMIT_PAYLOAD,
                        _ => 16,
                    };
                    let room = (log_end - block - RECORD_HEADER).min(length);
                    bytes[block + 4..block + 8].copy_from_slice(&(length as u32).to_le_bytes());
                    bytes[block + 8] = kind;
                    bytes[block + 9] = 0;
                    let payload = block + RECORD_HEADER;
                    rng.fill(&mut bytes[payload..payload + room]);
                    if kind == 4 || kind == 5 {
                        for field in 0..room / 8 {
                            let value = rng.below(64) as u64;
                            bytes[payload + 8 * field..payload + 8 * field + 8]
                                .copy_from_slice(&value.to_le_bytes());
                        }
                    }
                    let lsn = (block - log_start - LOG_BLOCK) as u64;
                    let seed = geometry.record_seed(1, lsn);
                    let end = block + RECORD_HEADER + room;
                    let checksum = crate::crc32c::crc32c_append(seed, &bytes[block + 4..end]);
                    bytes[block..block + 4].copy_from_slice(&checksum.to_le_bytes());
                }
                2 => {
                    let at = log_start + rng.below(log_end - log_start);
                    rng.fill(&mut bytes[at..(at + 512).min(log_end)]);
                }
                _ => {
                    let cut = 2 * 4096 + rng.below(bytes.len() - 2 * 4096);
                    bytes.truncate(cut);
                }
            }
            raw.set_size(0).unwrap();
            raw.write_all_at(&bytes, 0).unwrap();
            raw.sync_all().unwrap();
            for access in [Access::ReadOnly, Access::ReadWrite] {
                if let Ok(cache) = open(&fs, access) {
                    opened += 1;
                    let page = cache.superblock().free_list_head.max(18);
                    let _ = fill_of_any(&cache, page);
                }
            }
        }
        assert!(opened > 100);
    }

    fn fill_of_any(cache: &PageCache<SimFile>, page: u64) -> Option<u8> {
        cache.read(page).ok().map(|bytes| bytes[0])
    }
}
