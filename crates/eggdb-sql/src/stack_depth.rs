use crate::error::{SqlError, SqlResult, SqlState};
use std::cell::Cell;

const ASSUMED_THREAD_STACK_BYTES: usize = 2 << 20;
const HEADROOM_BYTES: usize = 512 << 10;

/// The stack each SQL session thread of EggDB's servers is given, and the size to give a thread
/// that runs deeply nested routines, triggers or Transact-SQL procedures.
pub const SESSION_STACK_BYTES: usize = 16 << 20;

thread_local! {
    static THREAD_STACK: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Tells the engine that the calling thread's stack holds `bytes` from here on, so nested
/// routines, triggers and Transact-SQL calls stop with SQLSTATE `54001` before they would
/// overflow it. Call it first thing on a thread made with that stack size; a thread that never
/// calls it is taken to have 2 MiB, Rust's default for spawned threads.
pub fn set_thread_stack_size(bytes: usize) {
    THREAD_STACK.with(|stack| stack.set(Some((stack_position(), bytes))));
}

pub(crate) fn check() -> SqlResult<()> {
    let here = stack_position();
    let (base, size) = THREAD_STACK.with(|stack| match stack.get() {
        Some(known) => known,
        None => {
            let assumed = (here, ASSUMED_THREAD_STACK_BYTES);
            stack.set(Some(assumed));
            assumed
        }
    });
    if base.abs_diff(here).saturating_add(HEADROOM_BYTES) > size {
        return Err(SqlError::new(
            SqlState::STATEMENT_TOO_COMPLEX,
            "stack depth limit exceeded",
        )
        .with_hint(
            "Nested routine, trigger and procedure calls used up this thread's stack; nest them less deeply, or run the session on a thread with eggdb_sql::SESSION_STACK_BYTES of stack.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check, set_thread_stack_size};

    fn nested(levels: usize) -> Result<(), String> {
        let padding = std::hint::black_box([0u8; 4096]);
        check().map_err(|error| error.state().code().to_owned())?;
        if levels > 0 {
            nested(levels - 1)?;
        }
        std::hint::black_box(padding);
        Ok(())
    }

    #[test]
    fn deep_nesting_stops_before_the_declared_stack_runs_out() {
        let outcome = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(|| {
                set_thread_stack_size(4 << 20);
                let shallow = nested(16);
                let deep = nested(100_000);
                (shallow, deep)
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(outcome.0, Ok(()));
        assert_eq!(outcome.1, Err("54001".to_owned()));
    }
}
