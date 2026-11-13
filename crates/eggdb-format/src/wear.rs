use crate::crc32c::crc32c;
use crate::flash::Flash;
use alloc::vec;
use alloc::vec::Vec;

const JOURNAL_MAGIC: [u8; 8] = *b"EggFTL01";
const RECORD_BYTES: usize = 24;
const HEADER_BYTES: usize = 24;
const UNMAPPED: u32 = u32::MAX;
const KIND_MAP: u8 = 1;
const KIND_ERASES: u8 = 2;
const KIND_SIZE: u8 = 3;
const STATIC_SPREAD: u32 = 64;

/// Why a wear-levelled flash region could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WearError<E> {
    /// The flash driver failed.
    Flash(E),
    /// The region is too small for its journal and one block, or its sectors are not a power
    /// of two.
    Geometry,
    /// A block number past the region's logical blocks.
    OutOfRange,
    /// No erased sector is left to write into.
    Full,
}

/// What a wear-levelled region has written since it was formatted or mounted (D1087).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WearStats {
    /// Blocks the caller wrote.
    pub blocks_written: u64,
    /// Data sectors programmed: one per block written, plus the blocks wear levelling moved.
    pub sectors_programmed: u64,
    /// Bytes of journal programmed: records, snapshots and headers.
    pub journal_bytes: u64,
    /// Sectors erased, journal slots included.
    pub erases: u64,
}

impl WearStats {
    /// Bytes programmed per byte the caller wrote, in thousandths: 1000 is none added. `None`
    /// before the first block is written.
    pub fn write_amplification_milli(&self, block_size: usize) -> Option<u64> {
        let written = self.blocks_written.checked_mul(block_size as u64).filter(|bytes| *bytes > 0)?;
        let programmed = self.sectors_programmed.saturating_mul(block_size as u64).saturating_add(self.journal_bytes);
        Some(programmed.saturating_mul(1000) / written)
    }
}

/// One journal record: a block mapped to a sector, a sector's erase count, or the region's
/// logical size, each with its sequence and checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    kind: u8,
    first: u32,
    second: u64,
    sequence: u32,
}

impl Record {
    fn encode(&self) -> [u8; RECORD_BYTES] {
        let mut out = [0xFFu8; RECORD_BYTES];
        out[0] = self.kind;
        out[4..8].copy_from_slice(&self.first.to_le_bytes());
        out[8..16].copy_from_slice(&self.second.to_le_bytes());
        out[16..20].copy_from_slice(&self.sequence.to_le_bytes());
        let check = crc32c(&out[..20]);
        out[20..24].copy_from_slice(&check.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; RECORD_BYTES] = bytes.try_into().ok()?;
        if bytes.iter().all(|byte| *byte == 0xFF) {
            return None;
        }
        let check = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
        if crc32c(&bytes[..20]) != check {
            return None;
        }
        Some(Self {
            kind: bytes[0],
            first: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            second: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            sequence: u32::from_le_bytes(bytes[16..20].try_into().ok()?),
        })
    }
}

/// Flash as numbered blocks of one erase sector each, wear-levelled and written atomically
/// (D1083): a block is rewritten into a fresh erased sector, the least-worn one, and only then
/// recorded in a journal, so a power cut at any moment leaves each block whole, old or new.
///
/// The region starts with two journal slots, each the same number of sectors, like a file's two
/// header slots: the active one opens with a header naming its generation and a snapshot of
/// every mapping, every sector's erase count and the logical size, then takes one record per
/// change; when it fills, the other slot is erased and opened with a fresh snapshot under the
/// next generation. Mounting reads the slot with the higher whole generation and replays its
/// records, passing over one a power cut tore. Data sectors are kept within 64 erases of each
/// other by moving cold blocks out of the least-worn sector.
#[derive(Debug)]
pub struct WearLevelled<F: Flash> {
    flash: F,
    sector: usize,
    slot_sectors: u64,
    data_sectors: u32,
    logical_blocks: u32,
    map: Vec<u32>,
    owner: Vec<u32>,
    erases: Vec<u32>,
    size: u64,
    active_slot: u64,
    generation: u64,
    journal_at: u64,
    sequence: u32,
    stats: WearStats,
}

impl<F: Flash> WearLevelled<F> {
    fn slot_bytes(&self) -> u64 {
        self.slot_sectors * self.sector as u64
    }

    fn data_offset(&self, physical: u32) -> u64 {
        (2 * self.slot_sectors + u64::from(physical)) * self.sector as u64
    }

    fn geometry(flash: &F, spare: u32) -> Result<(usize, u64, u32, u32), WearError<F::Error>> {
        let sector = flash.sector_size() as usize;
        if !sector.is_power_of_two() || sector < 256 {
            return Err(WearError::Geometry);
        }
        let sectors = flash.capacity() / sector as u64;
        let mut slot_sectors = 1u64;
        loop {
            let data = sectors.saturating_sub(2 * slot_sectors);
            let snapshot = HEADER_BYTES as u64 + (2 * data + 1) * RECORD_BYTES as u64;
            if snapshot * 2 <= slot_sectors * sector as u64 {
                let data = u32::try_from(data).map_err(|_| WearError::Geometry)?;
                let logical = data.checked_sub(spare.max(2)).filter(|logical| *logical > 0).ok_or(WearError::Geometry)?;
                return Ok((sector, slot_sectors, data, logical));
            }
            slot_sectors += 1;
            if 2 * slot_sectors >= sectors {
                return Err(WearError::Geometry);
            }
        }
    }

    /// Formats `flash` as an empty wear-levelled region, keeping `spare` sectors free (at least
    /// two) for rewrites and wear levelling.
    pub fn format(mut flash: F, spare: u32) -> Result<Self, WearError<F::Error>> {
        let (sector, slot_sectors, data_sectors, logical_blocks) = Self::geometry(&flash, spare)?;
        for index in 0..2 * slot_sectors {
            flash.erase(index * sector as u64).map_err(WearError::Flash)?;
        }
        let mut region = Self::empty(flash, sector, slot_sectors, data_sectors, logical_blocks);
        region.active_slot = 1;
        region.open_slot()?;
        Ok(region)
    }

    fn empty(flash: F, sector: usize, slot_sectors: u64, data_sectors: u32, logical_blocks: u32) -> Self {
        Self {
            flash,
            sector,
            slot_sectors,
            data_sectors,
            logical_blocks,
            map: vec![UNMAPPED; logical_blocks as usize],
            owner: vec![UNMAPPED; data_sectors as usize],
            erases: vec![0; data_sectors as usize],
            size: 0,
            active_slot: 0,
            generation: 0,
            journal_at: 0,
            sequence: 0,
            stats: WearStats::default(),
        }
    }

    /// Mounts a region `format` made, replaying its newest journal.
    pub fn mount(mut flash: F, spare: u32) -> Result<Self, WearError<F::Error>> {
        let (sector, slot_sectors, data_sectors, logical_blocks) = Self::geometry(&flash, spare)?;
        let slot_bytes = slot_sectors * sector as u64;
        let mut best: Option<(u64, u64)> = None;
        for slot in 0..2u64 {
            let mut header = [0u8; HEADER_BYTES];
            flash.read(slot * slot_bytes, &mut header).map_err(WearError::Flash)?;
            if header[..8] != JOURNAL_MAGIC || crc32c(&header[..20]) != u32::from_le_bytes([header[20], header[21], header[22], header[23]]) {
                continue;
            }
            let generation = u64::from_le_bytes(header[8..16].try_into().unwrap_or_default());
            if best.is_none_or(|(_, known)| generation > known) {
                best = Some((slot, generation));
            }
        }
        let Some((slot, generation)) = best else {
            return Err(WearError::Geometry);
        };
        let mut region = Self::empty(flash, sector, slot_sectors, data_sectors, logical_blocks);
        region.active_slot = slot;
        region.generation = generation;
        let mut at = HEADER_BYTES as u64;
        let mut record = [0u8; RECORD_BYTES];
        while at + RECORD_BYTES as u64 <= slot_bytes {
            region.flash.read(slot * slot_bytes + at, &mut record).map_err(WearError::Flash)?;
            if record.iter().all(|byte| *byte == 0xFF) {
                break;
            }
            if let Some(found) = Record::decode(&record) {
                region.apply(found);
                region.sequence = region.sequence.max(found.sequence);
            }
            at += RECORD_BYTES as u64;
        }
        region.journal_at = at;
        Ok(region)
    }

    fn apply(&mut self, record: Record) {
        match record.kind {
            KIND_MAP => {
                let (logical, physical) = (record.first, record.second as u32);
                if logical >= self.logical_blocks || (physical != UNMAPPED && physical >= self.data_sectors) {
                    return;
                }
                let previous = self.map[logical as usize];
                if previous != UNMAPPED && self.owner[previous as usize] == logical {
                    self.owner[previous as usize] = UNMAPPED;
                }
                self.map[logical as usize] = physical;
                if physical != UNMAPPED {
                    self.owner[physical as usize] = logical;
                }
            }
            KIND_ERASES if record.first < self.data_sectors => {
                self.erases[record.first as usize] = u32::try_from(record.second).unwrap_or(u32::MAX);
            }
            KIND_SIZE => self.size = record.second,
            _ => {}
        }
    }

    fn append(&mut self, kind: u8, first: u32, second: u64) -> Result<(), WearError<F::Error>> {
        if self.journal_at + RECORD_BYTES as u64 > self.slot_bytes() {
            self.open_slot()?;
        }
        self.sequence = self.sequence.wrapping_add(1);
        let record = Record { kind, first, second, sequence: self.sequence };
        let offset = self.active_slot * self.slot_bytes() + self.journal_at;
        self.flash.program(offset, &record.encode()).map_err(WearError::Flash)?;
        self.stats.journal_bytes += RECORD_BYTES as u64;
        self.journal_at += RECORD_BYTES as u64;
        self.apply(record);
        Ok(())
    }

    fn open_slot(&mut self) -> Result<(), WearError<F::Error>> {
        let next = 1 - self.active_slot;
        let base = next * self.slot_bytes();
        for index in 0..self.slot_sectors {
            self.flash.erase(base + index * self.sector as u64).map_err(WearError::Flash)?;
        }
        self.stats.erases += self.slot_sectors;
        let mut records = Vec::with_capacity(self.map.len() + self.erases.len() + 1);
        for (logical, physical) in self.map.iter().enumerate() {
            if *physical != UNMAPPED {
                records.push((KIND_MAP, logical as u32, u64::from(*physical)));
            }
        }
        for (physical, count) in self.erases.iter().enumerate() {
            if *count > 0 {
                records.push((KIND_ERASES, physical as u32, u64::from(*count)));
            }
        }
        records.push((KIND_SIZE, 0, self.size));
        let mut at = HEADER_BYTES as u64;
        for (kind, first, second) in records {
            self.sequence = self.sequence.wrapping_add(1);
            let record = Record { kind, first, second, sequence: self.sequence };
            self.flash.program(base + at, &record.encode()).map_err(WearError::Flash)?;
            at += RECORD_BYTES as u64;
        }
        self.stats.journal_bytes += at;
        let mut header = [0xFFu8; HEADER_BYTES];
        header[..8].copy_from_slice(&JOURNAL_MAGIC);
        header[8..16].copy_from_slice(&(self.generation + 1).to_le_bytes());
        header[16..20].copy_from_slice(&0u32.to_le_bytes());
        let check = crc32c(&header[..20]);
        header[20..24].copy_from_slice(&check.to_le_bytes());
        self.flash.program(base, &header).map_err(WearError::Flash)?;
        self.generation += 1;
        self.active_slot = next;
        self.journal_at = at;
        Ok(())
    }

    /// How many blocks the region holds for data.
    pub fn block_count(&self) -> u32 {
        self.logical_blocks
    }

    /// The size of one block, an erase sector.
    pub fn block_size(&self) -> usize {
        self.sector
    }

    /// The logical size kept with the region, which a file over it reports.
    pub fn logical_size(&self) -> u64 {
        self.size
    }

    /// Records a new logical size.
    pub fn set_logical_size(&mut self, size: u64) -> Result<(), WearError<F::Error>> {
        if size != self.size {
            self.append(KIND_SIZE, 0, size)?;
        }
        Ok(())
    }

    /// The fewest and most erases of any data sector.
    pub fn erase_spread(&self) -> (u32, u32) {
        let low = self.erases.iter().copied().min().unwrap_or(0);
        let high = self.erases.iter().copied().max().unwrap_or(0);
        (low, high)
    }

    /// Reads block `block` into `buffer` (one block long); a block never written, or trimmed,
    /// reads as zeros.
    pub fn read_block(&mut self, block: u32, buffer: &mut [u8]) -> Result<(), WearError<F::Error>> {
        if block >= self.logical_blocks || buffer.len() != self.sector {
            return Err(WearError::OutOfRange);
        }
        match self.map[block as usize] {
            UNMAPPED => {
                buffer.fill(0);
                Ok(())
            }
            physical => self.flash.read(self.data_offset(physical), buffer).map_err(WearError::Flash),
        }
    }

    fn least_worn_free(&self, excluding: u32) -> Option<u32> {
        (0..self.data_sectors)
            .filter(|physical| *physical != excluding && self.owner[*physical as usize] == UNMAPPED)
            .min_by_key(|physical| self.erases[*physical as usize])
    }

    fn place(&mut self, block: u32, data: &[u8]) -> Result<(), WearError<F::Error>> {
        let current = self.map[block as usize];
        let target = self.least_worn_free(current).ok_or(WearError::Full)?;
        let offset = self.data_offset(target);
        self.flash.erase(offset).map_err(WearError::Flash)?;
        self.stats.erases += 1;
        let count = self.erases[target as usize].saturating_add(1);
        self.append(KIND_ERASES, target, u64::from(count))?;
        self.flash.program(offset, data).map_err(WearError::Flash)?;
        self.stats.sectors_programmed += 1;
        self.append(KIND_MAP, block, u64::from(target))
    }

    /// Writes block `block` from `data` (one block long): into a fresh sector first, then
    /// recorded, so a power cut leaves the old block or the new one, never a mix.
    pub fn write_block(&mut self, block: u32, data: &[u8]) -> Result<(), WearError<F::Error>> {
        if block >= self.logical_blocks || data.len() != self.sector {
            return Err(WearError::OutOfRange);
        }
        self.place(block, data)?;
        self.stats.blocks_written += 1;
        self.level()
    }

    /// What the region has written since it was formatted or mounted.
    pub fn stats(&self) -> WearStats {
        self.stats
    }

    /// How many more blocks the caller can write before the data sectors reach
    /// `endurance_cycles` erases each, at the erases per block written seen so far (one before
    /// the first write): the flash-wear budget left (D1087).
    pub fn blocks_left(&self, endurance_cycles: u32) -> u64 {
        let erases_left: u64 = self.erases.iter().map(|count| u64::from(endurance_cycles.saturating_sub(*count))).sum();
        let erases_per_block_milli = match self.stats.blocks_written {
            0 => 1000,
            written => (self.stats.erases.saturating_mul(1000) / written).max(1000),
        };
        erases_left.saturating_mul(1000) / erases_per_block_milli
    }

    fn level(&mut self) -> Result<(), WearError<F::Error>> {
        let (low, high) = self.erase_spread();
        if high - low <= STATIC_SPREAD {
            return Ok(());
        }
        let Some(coldest) = (0..self.data_sectors).find(|physical| self.erases[*physical as usize] == low && self.owner[*physical as usize] != UNMAPPED) else {
            return Ok(());
        };
        let block = self.owner[coldest as usize];
        let mut data = vec![0u8; self.sector];
        self.flash.read(self.data_offset(coldest), &mut data).map_err(WearError::Flash)?;
        self.place(block, &data)
    }

    /// Forgets block `block`, which then reads as zeros, freeing its sector.
    pub fn trim_block(&mut self, block: u32) -> Result<(), WearError<F::Error>> {
        if block >= self.logical_blocks {
            return Err(WearError::OutOfRange);
        }
        if self.map[block as usize] != UNMAPPED {
            self.append(KIND_MAP, block, u64::from(UNMAPPED))?;
        }
        Ok(())
    }

    /// The flash driver, given back.
    pub fn into_inner(self) -> F {
        self.flash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::TestRng;

    #[derive(Debug, Clone, PartialEq)]
    enum ChipError {
        NotErased,
        Outside,
        PowerCut,
    }

    #[derive(Clone)]
    struct Chip {
        bytes: Vec<u8>,
        sector: u32,
        operations_left: Option<usize>,
    }

    impl Chip {
        fn spend(&mut self) -> Result<(), ChipError> {
            match &mut self.operations_left {
                Some(0) => Err(ChipError::PowerCut),
                Some(left) => {
                    *left -= 1;
                    Ok(())
                }
                None => Ok(()),
            }
        }
    }

    impl Flash for Chip {
        type Error = ChipError;

        fn capacity(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn sector_size(&self) -> u32 {
            self.sector
        }

        fn read(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), ChipError> {
            let start = offset as usize;
            buffer.copy_from_slice(self.bytes.get(start..start + buffer.len()).ok_or(ChipError::Outside)?);
            Ok(())
        }

        fn erase(&mut self, offset: u64) -> Result<(), ChipError> {
            let start = offset as usize;
            let sector = self.sector as usize;
            if self.operations_left == Some(0) {
                if let Some(target) = self.bytes.get_mut(start..start + sector / 2) {
                    target.fill(0xFF);
                }
                return Err(ChipError::PowerCut);
            }
            self.spend()?;
            self.bytes.get_mut(start..start + sector).ok_or(ChipError::Outside)?.fill(0xFF);
            Ok(())
        }

        fn program(&mut self, offset: u64, data: &[u8]) -> Result<(), ChipError> {
            let start = offset as usize;
            let cut = self.operations_left == Some(0);
            let target = self.bytes.get_mut(start..start + data.len()).ok_or(ChipError::Outside)?;
            if target.iter().zip(data).any(|(old, new)| old & new != *new) {
                return Err(ChipError::NotErased);
            }
            if cut {
                let half = data.len() / 2;
                target[..half].copy_from_slice(&data[..half]);
                return Err(ChipError::PowerCut);
            }
            self.spend()?;
            target.copy_from_slice(data);
            Ok(())
        }
    }

    fn chip(sectors: usize) -> Chip {
        Chip { bytes: vec![0xFF; sectors * 512], sector: 512, operations_left: None }
    }

    fn block(fill: u8) -> Vec<u8> {
        vec![fill; 512]
    }

    #[test]
    fn blocks_survive_remounts_and_journal_slots_turn_over() {
        let mut region = WearLevelled::format(chip(64), 4).unwrap();
        let blocks = region.block_count();
        assert!(blocks > 10);
        for round in 0..200u32 {
            let target = round % 5;
            region.write_block(target, &block(round as u8)).unwrap();
        }
        region.set_logical_size(4096).unwrap();
        let first_generation = region.generation;
        let mut region = WearLevelled::mount(region.into_inner(), 4).unwrap();
        assert_eq!(region.logical_size(), 4096);
        let mut read = block(0);
        for target in 0..5u32 {
            region.read_block(target, &mut read).unwrap();
            assert_eq!(read, block((195 + target) as u8), "block {target}");
        }
        assert!(first_generation > 1, "the journal turned over to its other slot");
    }

    #[test]
    fn writes_spread_wear_across_sectors() {
        let mut region = WearLevelled::format(chip(48), 4).unwrap();
        for target in 0..10u32 {
            region.write_block(target, &block(1)).unwrap();
        }
        for round in 0..2_000u32 {
            region.write_block(10, &block(round as u8)).unwrap();
        }
        let (low, high) = region.erase_spread();
        assert!(high - low <= STATIC_SPREAD + 2, "erases spread from {low} to {high}");
    }

    const SMALL_SECTOR_AMPLIFICATION_BUDGET_MILLI: u64 = 1400;
    const HOT_BLOCK_AMPLIFICATION_BUDGET_MILLI: u64 = 1600;

    #[test]
    fn random_writes_stay_within_the_write_amplification_budget() {
        let mut region = WearLevelled::format(chip(48), 4).unwrap();
        let mut rng = TestRng::new(1087);
        for round in 0..3_000u32 {
            let target = rng.below(region.block_count() as usize) as u32;
            region.write_block(target, &block(round as u8)).unwrap();
        }
        let stats = region.stats();
        assert_eq!(stats.blocks_written, 3_000);
        assert!(stats.sectors_programmed >= stats.blocks_written);
        let amplification = stats.write_amplification_milli(region.block_size()).unwrap();
        assert!(amplification <= SMALL_SECTOR_AMPLIFICATION_BUDGET_MILLI, "write amplification {amplification}");
        let (low, high) = region.erase_spread();
        assert!(high - low <= STATIC_SPREAD + 2, "erases spread from {low} to {high}");
    }

    #[test]
    fn one_hot_block_among_cold_ones_stays_within_its_budget() {
        let mut region = WearLevelled::format(chip(48), 4).unwrap();
        for target in 0..10u32 {
            region.write_block(target, &block(1)).unwrap();
        }
        for round in 0..4_000u32 {
            region.write_block(10, &block(round as u8)).unwrap();
        }
        let amplification = region.stats().write_amplification_milli(region.block_size()).unwrap();
        assert!(amplification <= HOT_BLOCK_AMPLIFICATION_BUDGET_MILLI, "write amplification {amplification}");
    }

    #[test]
    fn the_wear_budget_left_shrinks_as_blocks_are_written() {
        let mut region = WearLevelled::format(chip(48), 4).unwrap();
        assert_eq!(region.stats().write_amplification_milli(region.block_size()), None);
        let fresh = region.blocks_left(10_000);
        assert_eq!(fresh, u64::from(region.data_sectors) * 10_000);
        for round in 0..200u32 {
            region.write_block(round % 5, &block(round as u8)).unwrap();
        }
        let worn = region.blocks_left(10_000);
        assert!(worn < fresh);
        assert!(worn > fresh / 2);
        assert_eq!(region.blocks_left(0), 0);
    }

    #[test]
    fn a_power_cut_at_any_moment_leaves_each_block_old_or_new() {
        let mut rng = TestRng::new(7);
        let mut base = WearLevelled::format(chip(48), 4).unwrap();
        for target in 0..8u32 {
            base.write_block(target, &block(10 + target as u8)).unwrap();
        }
        let prepared = base.into_inner();
        for cut_after in 0..120usize {
            let mut flash = prepared.clone();
            flash.operations_left = Some(cut_after);
            let mut region = WearLevelled::mount(flash, 4).unwrap();
            let target = (rng.next_u64() % 8) as u32;
            let outcome = region.write_block(target, &block(200));
            let mut flash = region.into_inner();
            flash.operations_left = None;
            let mut region = WearLevelled::mount(flash, 4).unwrap();
            let mut read = block(0);
            for other in 0..8u32 {
                region.read_block(other, &mut read).unwrap();
                let old = block(10 + other as u8);
                if other == target {
                    assert!(read == old || read == block(200), "cut after {cut_after}: block {other} is torn");
                    if outcome.is_ok() {
                        assert_eq!(read, block(200), "cut after {cut_after}: a finished write was lost");
                    }
                } else {
                    assert_eq!(read, old, "cut after {cut_after}: block {other} changed");
                }
            }
        }
    }
}
