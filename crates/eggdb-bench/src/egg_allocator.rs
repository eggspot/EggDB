use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use eggdb_platform::EggAllocator;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;

pub(crate) const SMALL_BLOCKS: &str = "allocator.small_blocks";

static EGG: EggAllocator = EggAllocator::new();

const SIZES: [usize; 8] = [16, 24, 48, 100, 256, 700, 2_000, 9_000];
const LIVE: usize = 64;

fn churn(allocator: &dyn GlobalAlloc, iterations: u64) -> Result<(), String> {
    let layouts: Vec<Layout> = SIZES
        .iter()
        .map(|size| Layout::from_size_align(*size, 8).map_err(|error| error.to_string()))
        .collect::<Result<_, _>>()?;
    let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(LIVE);
    for round in 0..iterations {
        let layout = layouts[round as usize % layouts.len()];
        // SAFETY: every layout above has a non-zero size.
        let block = unsafe { allocator.alloc(layout) };
        if block.is_null() {
            return Err("an allocation failed".to_owned());
        }
        black_box(block);
        live.push((block, layout));
        if live.len() == LIVE {
            for (block, layout) in live.drain(..) {
                // SAFETY: each block was allocated just above with its layout.
                unsafe { allocator.dealloc(block, layout) };
            }
        }
    }
    for (block, layout) in live {
        // SAFETY: as above.
        unsafe { allocator.dealloc(block, layout) };
    }
    Ok(())
}

pub(crate) fn small_blocks(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let measured = measure::paired(
        &mut |iterations| churn(&EGG, iterations),
        &mut |iterations| churn(&System, iterations),
    )?;
    Ok(vec![
        Metric::ratio(SMALL_BLOCKS, "egg_to_system_allocator", measured.ratio),
        Metric::reported(SMALL_BLOCKS, "egg_ns_per_block", measured.subject_ns),
        Metric::reported(SMALL_BLOCKS, "system_ns_per_block", measured.reference_ns),
    ])
}
