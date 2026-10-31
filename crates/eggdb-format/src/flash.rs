use crate::error::FormatError;
use crate::format::{FileId, PageChecksum, PageHeader, PageSize};

/// Raw flash memory as its driver presents it: bytes read anywhere, erased a sector at a time to
/// `0xFF`, and programmed only where erased.
pub trait Flash {
    /// Why an operation failed.
    type Error;

    /// How many bytes the region holds.
    fn capacity(&self) -> u64;

    /// The size of one erase sector in bytes, a power of two.
    fn sector_size(&self) -> u32;

    /// Reads `buffer.len()` bytes starting at `offset`.
    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), Self::Error>;

    /// Erases the sector starting at `offset`, a multiple of [`Flash::sector_size`].
    fn erase(&mut self, offset: u64) -> Result<(), Self::Error>;

    /// Programs `data` at `offset` into bytes erased since they were last programmed.
    fn program(&mut self, offset: u64, data: &[u8]) -> Result<(), Self::Error>;
}

/// Why a page on flash could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlashPageError<E> {
    /// The flash driver failed.
    Flash(E),
    /// The page's bytes or header were refused, by the same checks a file's pages pass.
    Format(FormatError),
    /// The page lies beyond the flash region.
    OutOfRange {
        /// The page asked for.
        page: u64,
    },
    /// The page size is not a whole number of erase sectors, or the sector size is not a power
    /// of two.
    Geometry,
}

/// EggDB pages kept directly on flash: page `n` starts at byte `n` × the page size, written with
/// the same header and checksum as in a file, so a damaged, torn or misplaced page is refused
/// when read. Pages before [`PageSize::first_data_page`] are the header slots' room. Nothing is
/// allocated: every call works in the caller's page buffer.
#[derive(Debug)]
pub struct FlashPages<F: Flash> {
    flash: F,
    checksum: PageChecksum,
    page_size: PageSize,
}

impl<F: Flash> FlashPages<F> {
    /// Pages of `page_size` for the database `file_id` on `flash`.
    pub fn new(flash: F, file_id: FileId, page_size: PageSize) -> Result<Self, FlashPageError<F::Error>> {
        let sector = u64::from(flash.sector_size());
        if !sector.is_power_of_two() || page_size.bytes() as u64 % sector != 0 {
            return Err(FlashPageError::Geometry);
        }
        Ok(Self { flash, checksum: PageChecksum::new(file_id, page_size), page_size })
    }

    /// How many pages the flash region holds, header slots' room included.
    pub fn page_count(&self) -> u64 {
        self.flash.capacity() / self.page_size.bytes() as u64
    }

    fn offset(&self, page_id: u64) -> Result<u64, FlashPageError<F::Error>> {
        if page_id >= self.page_count() {
            return Err(FlashPageError::OutOfRange { page: page_id });
        }
        Ok(page_id * self.page_size.bytes() as u64)
    }

    /// Writes `header` and its checksum into `page` and stores it as page `page_id`, erasing the
    /// page's sectors first.
    pub fn write(&mut self, page_id: u64, header: &PageHeader, page: &mut [u8]) -> Result<(), FlashPageError<F::Error>> {
        let offset = self.offset(page_id)?;
        self.checksum.write(page, page_id, header).map_err(FlashPageError::Format)?;
        let sector = u64::from(self.flash.sector_size());
        let mut erased = 0;
        while erased < page.len() as u64 {
            self.flash.erase(offset + erased).map_err(FlashPageError::Flash)?;
            erased += sector;
        }
        self.flash.program(offset, page).map_err(FlashPageError::Flash)
    }

    /// Reads page `page_id` into `page` and returns its header once its checksum holds.
    pub fn read(&mut self, page_id: u64, page: &mut [u8]) -> Result<PageHeader, FlashPageError<F::Error>> {
        let offset = self.offset(page_id)?;
        if page.len() != self.page_size.bytes() {
            return Err(FlashPageError::Format(FormatError::WrongLength { expected: self.page_size.bytes(), actual: page.len() }));
        }
        self.flash.read(offset, page).map_err(FlashPageError::Flash)?;
        self.checksum.verify(page, page_id).map_err(FlashPageError::Format)
    }

    /// The flash driver, given back.
    pub fn into_inner(self) -> F {
        self.flash
    }
}

#[cfg(test)]
mod tests {
    use super::{Flash, FlashPageError, FlashPages};
    use crate::format::{FileId, PageHeader, PageSize, PageType};

    #[derive(Debug, PartialEq)]
    enum ChipError {
        NotErased,
        Outside,
    }

    struct Chip {
        bytes: Vec<u8>,
        sector: u32,
        erases: usize,
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
            let start = usize::try_from(offset).map_err(|_| ChipError::Outside)?;
            buffer.copy_from_slice(self.bytes.get(start..start + buffer.len()).ok_or(ChipError::Outside)?);
            Ok(())
        }

        fn erase(&mut self, offset: u64) -> Result<(), ChipError> {
            let start = usize::try_from(offset).map_err(|_| ChipError::Outside)?;
            self.bytes.get_mut(start..start + self.sector as usize).ok_or(ChipError::Outside)?.fill(0xFF);
            self.erases += 1;
            Ok(())
        }

        fn program(&mut self, offset: u64, data: &[u8]) -> Result<(), ChipError> {
            let start = usize::try_from(offset).map_err(|_| ChipError::Outside)?;
            let target = self.bytes.get_mut(start..start + data.len()).ok_or(ChipError::Outside)?;
            if target.iter().zip(data).any(|(old, new)| old & new != *new) {
                return Err(ChipError::NotErased);
            }
            target.copy_from_slice(data);
            Ok(())
        }
    }

    fn header(lsn: u64) -> PageHeader {
        PageHeader { page_type: PageType::BTreeLeaf, layout: 0, flags: 0, lsn }
    }

    #[test]
    fn pages_round_trip_on_flash_and_damage_is_refused() {
        let size = PageSize::DEFAULT;
        let chip = Chip { bytes: vec![0; size.bytes() * 8], sector: 1024, erases: 0 };
        let mut pages = FlashPages::new(chip, FileId([7; 16]), size).unwrap();
        let first = size.first_data_page();
        let mut page = vec![0u8; size.bytes()];
        page[100..105].copy_from_slice(b"hello");
        pages.write(first, &header(9), &mut page).unwrap();
        page[100..105].copy_from_slice(b"again");
        pages.write(first, &header(10), &mut page).unwrap();
        let mut read = vec![0u8; size.bytes()];
        assert_eq!(pages.read(first, &mut read).unwrap(), header(10));
        assert_eq!(&read[100..105], b"again");
        assert!(matches!(pages.read(first + 1, &mut read), Err(FlashPageError::Format(_))));
        assert_eq!(pages.read(8, &mut read), Err(FlashPageError::OutOfRange { page: 8 }));
        assert!(matches!(pages.write(0, &header(1), &mut page), Err(FlashPageError::Format(_))));
        let mut chip = pages.into_inner();
        assert_eq!(chip.erases, 2 * size.bytes() / 1024);
        chip.bytes[first as usize * size.bytes() + 200] ^= 1;
        let mut pages = FlashPages::new(chip, FileId([7; 16]), size).unwrap();
        assert!(matches!(pages.read(first, &mut read), Err(FlashPageError::Format(_))));
        let other_file = FlashPages::new(pages.into_inner(), FileId([8; 16]), size);
        assert!(other_file.is_ok());
        let odd = Chip { bytes: vec![0; 10_000], sector: 3000, erases: 0 };
        assert!(matches!(FlashPages::new(odd, FileId([0; 16]), size), Err(FlashPageError::Geometry)));
    }
}
