# Test coverage of the core

EggDB publishes one coverage figure for its core: the share of branches in `eggdb-format`, `eggdb-storage` and the
`eggdb` library that the unit tests take (D1133). SQLite's own tests take every branch (100% branch coverage, and
MC/DC); EggDB's target rises toward that every release.

- **Where the target is.** `benchmarks/coverage.txt`, one line: `core branch_coverage_percent <target>`.
- **How it is measured.** `scripts/coverage.sh` builds the core's unit tests with the compiler's own instrumentation
  (`-C instrument-coverage`, with branch counters), runs them, and reads the totals with the `llvm-cov` that comes
  with the pinned toolchain's `llvm-tools` component. No third-party tool is involved. Test files and code outside
  the repository are left out.
- **When.** The `Coverage` workflow runs it every Saturday, beside the weekly performance pass, and on request. It
  fails when the figure is below the target, and puts the figures in the run's summary and the full report in its
  artifacts.
- **Rising every release.** Each minor release sets the target to the last Saturday's figure rounded down, and never
  lowers it. The release notes state the figure. The first run sets the starting target (`--update`).
