//! The behavior every `Cpus` shares, checked against the operating system and the simulation.

use eggdb_platform::{CACHE_LINE_PADDING, CpuId, CpuSet, Cpus, ErrorKind, OsCpus, SimCpus};

macro_rules! contract {
    ($name:ident, |$cpus:ident| $body:block) => {
        #[test]
        fn $name() {
            fn run<C: Cpus + Clone + 'static>($cpus: &C) $body
            run(&OsCpus::new());
            run(&SimCpus::new());
        }
    };
}

contract!(some_cpus_are_available, |cpus| {
    let available = cpus.available();
    assert!(!available.is_empty());
    assert!(cpus.parallelism().get() <= available.len());
});

contract!(empty_and_foreign_sets_are_refused, |cpus| {
    let error = cpus.pin_current_thread(&CpuSet::new()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    let last = cpus.available().iter().last().unwrap().index();
    if let Ok(beyond) = CpuId::new(last + 1) {
        let foreign: CpuSet = [beyond].into_iter().collect();
        let error = cpus.pin_current_thread(&foreign).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
});

contract!(a_thread_is_pinned_or_refused_never_faked, |cpus| {
    let cpus = cpus.clone();
    std::thread::spawn(move || {
        let one: CpuSet = cpus.available().iter().take(1).collect();
        match cpus.pin_current_thread(&one) {
            Ok(()) => {}
            Err(error) => assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}"),
        }
    })
    .join()
    .unwrap();
});

contract!(the_topology_agrees_with_the_available_cpus, |cpus| {
    let topology = cpus.topology();
    let available = cpus.available();
    if !topology.locations().is_empty() {
        assert!(available.is_subset(&topology.cpus()), "{topology:?}");
    }
    if let Some(logical) = topology.logical_cpus() {
        assert!(logical.get() >= available.len(), "{topology:?}");
    }
    if let Some(line) = topology.cache_line_size() {
        assert!(line.get() <= CACHE_LINE_PADDING, "{topology:?}");
    }
    assert_eq!(cpus.topology(), topology);
});
