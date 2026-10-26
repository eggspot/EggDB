use crate::bytes::{read_u32, read_u64, write_u32, write_u64};
use crate::error::{FormatError, StorageError};
use crate::format::{NO_PAGE, PAGE_HEADER_SIZE, PageHeader, PageSize, PageType};
use crate::page_cache::{PageCache, PageSource, TreePage};
use crate::page_set::PageSet;
use eggdb_platform::{Error, ErrorKind, File};

pub(crate) const LAYOUT: u8 = 1;
pub(crate) const REFERENCE_BYTES: usize = 16;
pub(crate) const MAX_VALUE_BYTES: usize = 1 << 30;

const NEXT: usize = 0;
const FIRST: usize = 8;
const INDEX: usize = 16;
const BYTES_HERE: usize = 20;
const DATA: usize = 24;

const HEADER: PageHeader = PageHeader {
    page_type: PageType::Overflow,
    layout: LAYOUT,
    flags: 0,
    lsn: 0,
};

pub(crate) fn data_bytes_per_page(page_size: PageSize) -> usize {
    page_size.bytes() - PAGE_HEADER_SIZE - DATA
}

pub(crate) fn pages_for(value_bytes: u64, page_size: PageSize) -> u64 {
    value_bytes.div_ceil(data_bytes_per_page(page_size) as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverflowReference {
    pub(crate) value_bytes: u64,
    pub(crate) first_page: u64,
}

impl OverflowReference {
    pub(crate) fn encode(&self) -> [u8; REFERENCE_BYTES] {
        let mut bytes = [0; REFERENCE_BYTES];
        write_u64(&mut bytes, 0, self.value_bytes);
        write_u64(&mut bytes, 8, self.first_page);
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() != REFERENCE_BYTES {
            return Err("a large value's reference is not 16 bytes");
        }
        let reference = Self {
            value_bytes: read_u64(bytes, 0),
            first_page: read_u64(bytes, 8),
        };
        if reference.value_bytes == 0 || reference.value_bytes > MAX_VALUE_BYTES as u64 {
            return Err("a large value's length is zero or above the largest value allowed");
        }
        if reference.first_page == NO_PAGE {
            return Err("a large value's first page is zero");
        }
        Ok(reference)
    }
}

pub(crate) fn write_value<F: File>(
    cache: &PageCache<F>,
    value: &[u8],
) -> Result<OverflowReference, StorageError> {
    let mut chunks = value.chunks(data_bytes_per_page(cache.page_size()));
    let first_chunk = chunks.next().unwrap_or_default();
    let mut page = cache.allocate(HEADER)?;
    let first_page = page.page_id();
    fill(&mut page, first_page, 0, first_chunk);
    for (index, chunk) in (1u32..).zip(chunks) {
        let mut next = cache.allocate(HEADER)?;
        write_u64(&mut page, NEXT, next.page_id());
        fill(&mut next, first_page, index, chunk);
        page = next;
    }
    Ok(OverflowReference {
        value_bytes: value.len() as u64,
        first_page,
    })
}

fn fill(body: &mut [u8], first_page: u64, index: u32, chunk: &[u8]) {
    write_u64(body, FIRST, first_page);
    write_u32(body, INDEX, index);
    write_u32(body, BYTES_HERE, chunk.len() as u32);
    body[DATA..DATA + chunk.len()].copy_from_slice(chunk);
}

pub(crate) fn read_value(
    pages: &impl PageSource,
    reference: &OverflowReference,
    value: &mut Vec<u8>,
) -> Result<(), StorageError> {
    let length = checked_length(pages, reference)?;
    value
        .try_reserve_exact(length)
        .map_err(|_| StorageError::Io(Error::new(ErrorKind::OutOfMemory)))?;
    walk(pages, reference, |_, data| {
        value.extend_from_slice(data);
        Ok(())
    })
}

pub(crate) fn value_pages(
    pages: &impl PageSource,
    reference: &OverflowReference,
) -> Result<Vec<u64>, StorageError> {
    checked_length(pages, reference)?;
    let count = pages_for(reference.value_bytes, pages.page_size()) as usize;
    let mut found = Vec::new();
    found
        .try_reserve_exact(count)
        .map_err(|_| StorageError::Io(Error::new(ErrorKind::OutOfMemory)))?;
    walk(pages, reference, |page, _| {
        found.push(page);
        Ok(())
    })?;
    Ok(found)
}

pub(crate) fn free_value<F: File>(
    cache: &PageCache<F>,
    reference: &OverflowReference,
) -> Result<(), StorageError> {
    for page in value_pages(cache, reference)? {
        cache.free(page)?;
    }
    Ok(())
}

pub(crate) fn check_value(
    pages: &impl PageSource,
    reference: &OverflowReference,
    used: &mut PageSet,
) -> Result<u64, StorageError> {
    checked_length(pages, reference)?;
    let mut count = 0;
    walk(pages, reference, |page, _| {
        if !used.insert(page)? {
            return Err(corrupt(page, "the page is reached twice"));
        }
        count += 1;
        Ok(())
    })?;
    Ok(count)
}

fn checked_length(
    pages: &impl PageSource,
    reference: &OverflowReference,
) -> Result<usize, StorageError> {
    let page_count = pages_for(reference.value_bytes, pages.page_size());
    if reference.value_bytes > MAX_VALUE_BYTES as u64 || page_count > pages.page_count() {
        return Err(corrupt(
            reference.first_page,
            "the value is longer than the file could hold",
        ));
    }
    Ok(reference.value_bytes as usize)
}

fn walk(
    pages: &impl PageSource,
    reference: &OverflowReference,
    mut visit: impl FnMut(u64, &[u8]) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    let per_page = data_bytes_per_page(pages.page_size()) as u64;
    let page_count = pages_for(reference.value_bytes, pages.page_size());
    let mut page_id = reference.first_page;
    let mut remaining = reference.value_bytes;
    for index in 0..page_count {
        let page = pages.page(page_id)?;
        let chunk = remaining.min(per_page) as usize;
        let last = index + 1 == page_count;
        let next = check_page(&page, reference, index, chunk, last)?;
        visit(page_id, &page[DATA..DATA + chunk])?;
        remaining -= chunk as u64;
        page_id = next;
    }
    Ok(())
}

fn check_page(
    page: &impl TreePage,
    reference: &OverflowReference,
    index: u64,
    chunk: usize,
    last: bool,
) -> Result<u64, StorageError> {
    let page_id = page.page_id();
    let header = page.header();
    if header.page_type != PageType::Overflow {
        return Err(corrupt(
            page_id,
            "a large value leads to a page of another type",
        ));
    }
    if header.layout != LAYOUT {
        return Err(corrupt(page_id, "the page has an unknown layout"));
    }
    if read_u64(page, FIRST) != reference.first_page || u64::from(read_u32(page, INDEX)) != index {
        return Err(corrupt(
            page_id,
            "the page belongs to another value or another place in the chain",
        ));
    }
    if read_u32(page, BYTES_HERE) as usize != chunk {
        return Err(corrupt(
            page_id,
            "the page holds another number of the value's bytes than the value's length says",
        ));
    }
    let next = read_u64(page, NEXT);
    if last {
        if next != NO_PAGE {
            return Err(corrupt(page_id, "the value's last page names a next page"));
        }
        if page[DATA + chunk..].iter().any(|&byte| byte != 0) {
            return Err(corrupt(
                page_id,
                "the bytes after the value's end are not zero",
            ));
        }
    } else if next == NO_PAGE {
        return Err(corrupt(page_id, "the chain ends before the value does"));
    }
    Ok(next)
}

fn corrupt(page: u64, problem: &'static str) -> StorageError {
    FormatError::CorruptOverflow { page, problem }.into()
}
