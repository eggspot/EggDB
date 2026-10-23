//! The behavior every `Memory` shares, checked against the operating system and the simulation.

use eggdb_platform::{ErrorKind, Memory, OsMemory, Pages, SimMemory};

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

contract!(the_page_size_is_a_power_of_two, |memory| {
    let page_size = memory.page_size();
    assert!(page_size.is_power_of_two());
    assert!(page_size >= 512);
    if let Some(huge) = memory.huge_page_size() {
        assert!(huge.is_power_of_two());
        assert!(huge > page_size);
    }
});

contract!(regions_are_page_aligned_zeroed_and_rounded_up, |memory| {
    let page_size = memory.page_size();
    for len in [1, page_size - 1, page_size, page_size + 1, 5 * page_size] {
        let region = memory.allocate(len, Pages::Normal).unwrap();
        assert_eq!(region.len(), len.next_multiple_of(page_size));
        assert!(region.as_ptr().addr().is_multiple_of(page_size));
        assert!(region.iter().all(|&byte| byte == 0));
        assert_eq!(region.pages(), Pages::Normal);
    }
});

contract!(regions_are_writable_and_independent, |memory| {
    let page_size = memory.page_size();
    let mut first = memory.allocate(2 * page_size, Pages::Normal).unwrap();
    let mut second = memory.allocate(page_size, Pages::Normal).unwrap();
    first.fill(0xAB);
    second[0] = 1;
    let last = first.len() - 1;
    assert_eq!((first[0], first[last]), (0xAB, 0xAB));
    assert!(second[1..].iter().all(|&byte| byte == 0));
});

contract!(zero_and_unroundable_lengths_are_invalid, |memory| {
    for len in [0, usize::MAX] {
        let error = memory.allocate(len, Pages::Normal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
});

contract!(huge_pages_are_given_or_refused_never_faked, |memory| {
    match (memory.huge_page_size(), memory.allocate(1, Pages::Huge)) {
        (Some(huge), Ok(region)) => {
            assert_eq!(region.len(), huge);
            assert!(region.as_ptr().addr().is_multiple_of(huge));
            assert_eq!(region.pages(), Pages::Huge);
        }
        (_, Err(error)) => assert_eq!(error.kind(), ErrorKind::Unsupported),
        (None, Ok(_)) => panic!("huge pages were handed out with no huge page size"),
    }
});

contract!(regions_move_between_threads, |memory| {
    let mut region = memory.allocate(1, Pages::Normal).unwrap();
    region[3] = 9;
    let read = std::thread::spawn(move || region[3]).join().unwrap();
    assert_eq!(read, 9);
});
