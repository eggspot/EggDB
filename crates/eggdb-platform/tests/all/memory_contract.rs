//! The behavior every `Memory` shares, checked against the operating system and the simulation.

use eggdb_platform::{ErrorKind, Memory, OsMemory, PageMode, SimMemory};

macro_rules! contract {
    ($name:ident, |$memory:ident| $body:block) => {
        #[test]
        fn $name() {
            fn run<M: Memory>($memory: &M) $body
            run(&OsMemory::new());
            run(&SimMemory::new());
        }
    };
}

contract!(page_sizes_are_powers_of_two, |memory| {
    let page_size = memory.page_size();
    assert!(page_size.is_power_of_two());
    assert!(page_size >= 512);
    if let Some(huge) = memory.huge_page_size() {
        assert!(huge.is_power_of_two());
        assert!(huge > page_size);
    }
});

contract!(regions_are_page_aligned_and_zeroed, |memory| {
    let page_size = memory.page_size();
    for pages in [1, 2, 5] {
        let region = memory
            .allocate(pages * page_size, PageMode::Normal)
            .unwrap();
        assert_eq!(region.len(), pages * page_size);
        assert!(region.as_ptr().addr().is_multiple_of(page_size));
        assert!(region.iter().all(|&byte| byte == 0));
        assert_eq!(region.mode(), PageMode::Normal);
    }
});

contract!(regions_are_writable_and_independent, |memory| {
    let page_size = memory.page_size();
    let mut first = memory.allocate(2 * page_size, PageMode::Normal).unwrap();
    let mut second = memory.allocate(page_size, PageMode::Normal).unwrap();
    first.fill(0xAB);
    second[0] = 1;
    let last = first.len() - 1;
    assert_eq!((first[0], first[last]), (0xAB, 0xAB));
    assert!(second[1..].iter().all(|&byte| byte == 0));
});

contract!(
    lengths_that_are_zero_or_not_whole_pages_are_invalid,
    |memory| {
        let page_size = memory.page_size();
        for len in [0, 1, page_size + 1, usize::MAX] {
            let error = memory.allocate(len, PageMode::Normal).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{len}");
        }
        if let Some(huge) = memory.huge_page_size() {
            for mode in [PageMode::Huge, PageMode::TransparentHuge] {
                let error = memory.allocate(page_size, mode).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::InvalidInput, "{mode:?}");
                let error = memory.allocate(huge + page_size, mode).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::InvalidInput, "{mode:?}");
            }
        }
    }
);

contract!(huge_pages_are_given_or_refused_never_faked, |memory| {
    for mode in [PageMode::Huge, PageMode::TransparentHuge] {
        let Some(huge) = memory.huge_page_size() else {
            let error = memory.allocate(1 << 21, mode).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{mode:?}");
            continue;
        };
        match memory.allocate(huge, mode) {
            Ok(mut region) => {
                assert_eq!(region.len(), huge);
                assert!(region.as_ptr().addr().is_multiple_of(huge));
                assert!(region.iter().all(|&byte| byte == 0));
                assert_eq!(region.mode(), mode);
                region.fill(7);
                assert_eq!(region[huge - 1], 7);
            }
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    ErrorKind::Unsupported | ErrorKind::OutOfMemory
                ),
                "{mode:?}: {error}"
            ),
        }
    }
});

contract!(regions_move_between_threads, |memory| {
    let mut region = memory
        .allocate(memory.page_size(), PageMode::Normal)
        .unwrap();
    region[3] = 9;
    let read = std::thread::spawn(move || region[3]).join().unwrap();
    assert_eq!(read, 9);
});
