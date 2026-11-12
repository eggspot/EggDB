use crate::args;
use crate::sql::{
    Backend, EggdbResult, EggdbSql, Holder, RemoteSession, ResultBatch, run_on, session_state,
};
use crate::status::{EGGDB_ERR_OUT_OF_MEMORY, Failure, guarded};
use eggdb::sql::{CopyChannel, CopyStart, SqlError, SqlResult, SqlState};
use eggdb_pgwire::client::{CopyOutPiece, QueryRows};
use std::ffi::{c_char, c_int};

pub(crate) const COPY_PIECE_BYTES: usize = 64 * 1024;

const FREED_BEFORE_ITS_END: &str = "the client freed the COPY before its end";

struct CopyInData<'a> {
    remaining: &'a [u8],
    started: bool,
}

fn not_a_copy(wanted: &str) -> Failure {
    Failure::sql(&SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!(
            "the statement ran, but it is not a {wanted}; run it with eggdb_sql_execute instead"
        ),
    ))
}

fn copy_out_refused() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "COPY ... TO STDOUT is not read through a copy-in; read its data with eggdb_sql_copy_out_begin and eggdb_copy_read",
    )
}

impl CopyChannel for CopyInData<'_> {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        if self.started {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "the data goes to one COPY ... FROM STDIN; run each COPY in a call of its own",
            ));
        }
        self.started = true;
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if self.remaining.is_empty() {
            return Ok(false);
        }
        let (piece, rest) = self
            .remaining
            .split_at(self.remaining.len().min(COPY_PIECE_BYTES));
        data.extend_from_slice(piece);
        self.remaining = rest;
        Ok(true)
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(copy_out_refused())
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Err(copy_out_refused())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct GatheredCopyOut {
    data: Vec<u8>,
    started: bool,
}

impl CopyChannel for GatheredCopyOut {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "COPY ... FROM STDIN takes its data from eggdb_sql_copy_in_begin and eggdb_copy_put, not from eggdb_sql_copy_out_begin",
        ))
    }

    fn read_copy_data(&mut self, _data: &mut Vec<u8>) -> SqlResult<bool> {
        Ok(false)
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        if self.started {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "eggdb_sql_copy_out_begin reads one COPY ... TO STDOUT; run each COPY in a call of its own",
            ));
        }
        self.started = true;
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        self.data.try_reserve(data.len()).map_err(|_| {
            SqlError::new(
                SqlState::OUT_OF_MEMORY,
                "out of memory while gathering the COPY's data",
            )
        })?;
        self.data.extend_from_slice(data);
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

enum Stage {
    LocalIn { text: String, data: Vec<u8> },
    LocalOut { data: Vec<u8>, read: usize },
    RemoteIn,
    RemoteOut,
    RemoteOutDrained,
    Ended,
}

impl Stage {
    fn name(&self) -> &'static str {
        match self {
            Self::LocalIn { .. } | Self::RemoteIn => "loading (COPY ... FROM STDIN)",
            Self::LocalOut { .. } | Self::RemoteOut | Self::RemoteOutDrained => {
                "reading out (COPY ... TO STDOUT)"
            }
            Self::Ended => "ended",
        }
    }
}

/// A `COPY` in progress on a SQL session (`eggdb_copy` in `eggdb.h`): a `COPY ... FROM STDIN`
/// from [`eggdb_sql_copy_in_begin`] fed by [`eggdb_copy_put`], or a `COPY ... TO STDOUT` from
/// [`eggdb_sql_copy_out_begin`] read by [`eggdb_copy_read`]; ended by [`eggdb_copy_end`] and
/// freed by [`eggdb_copy_free`]. It belongs to its session: a call on it counts as a call on
/// the session, the session runs nothing else until it ends, and the session cannot close
/// while it lives.
pub struct EggdbCopy {
    session: *mut EggdbSql,
    id: u64,
    stage: Stage,
    piece: Vec<u8>,
    results: ResultBatch,
}

impl std::fmt::Debug for EggdbCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EggdbCopy")
            .field("id", &self.id)
            .field("stage", &self.stage.name())
            .finish_non_exhaustive()
    }
}

fn wrong_stage(stage: &Stage, wanted: &str) -> Failure {
    Failure::invalid_argument(format!("this COPY is {}; {wanted}", stage.name()))
}

pub(crate) fn copy_in_remote(
    remote: &mut RemoteSession,
    batch: &mut ResultBatch,
    text: &str,
    data: &[u8],
) -> Result<(*const EggdbResult, usize), Failure> {
    batch.clear();
    let started = remote.ready_client()?.start_copy_in(text);
    started.map_err(|error| remote.failed(error))?;
    send_pieces(remote, data)?;
    finish_remote_copy_in(remote, batch)
}

fn send_pieces(remote: &mut RemoteSession, data: &[u8]) -> Result<(), Failure> {
    for piece in data.chunks(COPY_PIECE_BYTES) {
        let sent = remote.client()?.send_copy_data(piece);
        sent.map_err(|error| remote.failed(error))?;
    }
    Ok(())
}

fn finish_remote_copy_in(
    remote: &mut RemoteSession,
    batch: &mut ResultBatch,
) -> Result<(*const EggdbResult, usize), Failure> {
    let finished = remote.client()?.finish_copy_in();
    let gathered = finished.map_err(|error| remote.failed(error))?;
    batch.clear();
    for result in &gathered {
        batch.add_remote(result);
    }
    Ok(batch.publish())
}

fn publish_tag(batch: &mut ResultBatch, tag: String) {
    batch.clear();
    batch.add_remote(&QueryRows {
        tag,
        ..QueryRows::default()
    });
    batch.publish();
}

unsafe fn copy_handle<'a>(copy: *mut EggdbCopy) -> Result<&'a mut EggdbCopy, Failure> {
    // SAFETY: eggdb.h asks for a live copy handle used by this call alone.
    unsafe { args::handle_mut(copy, "copy") }
}

unsafe fn begin(
    sql: *mut EggdbSql,
    text: *const c_char,
    copy: *mut *mut EggdbCopy,
    loading: bool,
) -> Result<(), Failure> {
    // SAFETY: eggdb.h asks for writable memory for one handle at `copy`.
    unsafe { args::write(copy, std::ptr::null_mut(), "copy") }?;
    // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
    let state = unsafe { session_state(sql, "sql") }?;
    // SAFETY: eggdb.h asks for a NUL-terminated statement text.
    let text = unsafe { args::text(text, "text") }?;
    state.ensure_free()?;
    let id = state.next_handle_id();
    let mut made = EggdbCopy {
        session: sql,
        id,
        stage: Stage::Ended,
        piece: Vec::new(),
        results: ResultBatch::default(),
    };
    let in_the_process = matches!(state.backend, Backend::Local { .. });
    made.stage = match (in_the_process, loading) {
        (true, true) => Stage::LocalIn {
            text: text.to_owned(),
            data: Vec::new(),
        },
        (true, false) => {
            let mut channel = GatheredCopyOut::default();
            let gatherer: &mut dyn CopyChannel = &mut channel;
            run_on(
                &mut state.backend,
                &mut made.results,
                text,
                None,
                Some(gatherer),
            )?;
            if !channel.started {
                return Err(not_a_copy("COPY ... TO STDOUT"));
            }
            Stage::LocalOut {
                data: channel.data,
                read: 0,
            }
        }
        (false, true) => {
            let remote = state.remote()?;
            let started = remote.ready_client()?.start_copy_in(text);
            started.map_err(|error| remote.failed(error))?;
            Stage::RemoteIn
        }
        (false, false) => {
            let remote = state.remote()?;
            let started = remote.ready_client()?.start_copy_out(text);
            started.map_err(|error| remote.failed(error))?;
            Stage::RemoteOut
        }
    };
    state.hold(Holder::Copy(id));
    // SAFETY: as for `session_state` above; this only counts the copy.
    unsafe { args::handle(sql, "sql") }?.statement_prepared();
    let handle = args::give(made);
    // SAFETY: as for the first write to `copy`.
    unsafe { args::write(copy, handle, "copy") }
}

/// Starts the one `COPY ... FROM STDIN` in `text` (UTF-8, NUL-terminated) on the session and
/// puts a handle for it in `*copy` (NULL on failure). Give it its data with [`eggdb_copy_put`]
/// in pieces of any size, cut anywhere, even inside a row or a character, then end it with
/// [`eggdb_copy_end`]: the `COPY` loads every row or none. On a session from
/// `eggdb_sql_connect` each piece goes to the server as it is put, and a missing table fails
/// here; on a session in the process the pieces are kept until `eggdb_copy_end`, which runs the
/// `COPY` and reports its errors. A statement that is not a `COPY ... FROM STDIN` runs, then
/// fails with `EGGDB_ERR_SQL` and SQLSTATE `0A000` (here on a server, at `eggdb_copy_end` in the
/// process).
/// Until the copy ends, other calls that run statements on the session fail with
/// `EGGDB_ERR_IN_USE`.
///
/// # Safety
/// `sql` is a handle from `eggdb_sql_open` or `eggdb_sql_connect` used by this call alone,
/// `text` a NUL-terminated string, and `copy` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_copy_in_begin(
    sql: *mut EggdbSql,
    text: *const c_char,
    copy: *mut *mut EggdbCopy,
) -> c_int {
    // SAFETY: the caller's promises are the ones `begin` needs.
    guarded(|| unsafe { begin(sql, text, copy, true) })
}

/// Starts the one `COPY ... TO STDOUT` in `text` (UTF-8, NUL-terminated) on the session and
/// puts a handle for it in `*copy` (NULL on failure); read its data with [`eggdb_copy_read`]
/// in the text, CSV or binary format the `COPY`'s options name. On a session from
/// `eggdb_sql_connect` the data is read from the server as it is read from the handle; on a
/// session in the process the `COPY` runs here and its data waits in the handle. A statement
/// that is not a `COPY ... TO STDOUT` runs, then fails with `EGGDB_ERR_SQL` and SQLSTATE
/// `0A000`. Until the copy ends, other calls that run
/// statements on the session fail with `EGGDB_ERR_IN_USE`.
///
/// # Safety
/// As for `eggdb_sql_copy_in_begin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_copy_out_begin(
    sql: *mut EggdbSql,
    text: *const c_char,
    copy: *mut *mut EggdbCopy,
) -> c_int {
    // SAFETY: the caller's promises are the ones `begin` needs.
    guarded(|| unsafe { begin(sql, text, copy, false) })
}

/// Gives the next `len` bytes at `data` to a `COPY ... FROM STDIN` started by
/// [`eggdb_sql_copy_in_begin`]. The bytes are copied or sent before the call returns, so the
/// buffer may change right after it; a length of 0 does nothing. A copy that reads out, or
/// has ended, fails with `EGGDB_ERR_INVALID_ARGUMENT`; a connection that breaks ends the copy
/// with `EGGDB_ERR_IO`.
///
/// # Safety
/// `copy` is a handle from `eggdb_sql_copy_in_begin` used by this call alone, with no other
/// call on its session meanwhile (but `eggdb_sql_cancel`), and `data` is NULL with a length of
/// 0 or points to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_copy_put(
    copy: *mut EggdbCopy,
    data: *const u8,
    len: usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live copy handle used by this call alone.
        let copy = unsafe { copy_handle(copy) }?;
        // SAFETY: eggdb.h asks for NULL with 0 or `len` readable bytes at `data`.
        let data = unsafe { args::bytes(data, len, "data") }?;
        match &mut copy.stage {
            Stage::LocalIn { data: kept, .. } => {
                kept.try_reserve(data.len()).map_err(|_| {
                    Failure::new(
                        EGGDB_ERR_OUT_OF_MEMORY,
                        "out of memory while keeping the COPY's data; end this COPY and load the rest in another",
                    )
                })?;
                kept.extend_from_slice(data);
                Ok(())
            }
            Stage::RemoteIn => {
                // SAFETY: a live copy's session is open (it refuses to close while copies
                // live), and eggdb.h asks that no other call uses the session meanwhile.
                let state = unsafe { session_state(copy.session, "the copy's session") }?;
                let sent = send_pieces(state.remote()?, data);
                if sent.is_err() {
                    copy.stage = Stage::Ended;
                    state.release(Holder::Copy(copy.id));
                }
                sent
            }
            other => Err(wrong_stage(
                other,
                "eggdb_copy_put gives data to a copy from eggdb_sql_copy_in_begin before its end",
            )),
        }
    })
}

/// Puts the next piece of a `COPY ... TO STDOUT` started by [`eggdb_sql_copy_out_begin`] in
/// `*data` and its length in `*len`, or NULL and 0 once the data has ended. In the process a
/// piece is at most 64 KiB; from a server it is the server's messages gathered until they
/// reach 64 KiB (whole rows in the text and CSV formats). The piece belongs to
/// the copy until its next call. A failing `COPY` on a server fails here with `EGGDB_ERR_SQL`
/// and ends the copy.
///
/// # Safety
/// `copy` is a handle from `eggdb_sql_copy_out_begin` used by this call alone, with no other
/// call on its session meanwhile (but `eggdb_sql_cancel`), and `data` and `len` point to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_copy_read(
    copy: *mut EggdbCopy,
    data: *mut *const u8,
    len: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `data`.
        unsafe { args::write(data, std::ptr::null(), "data") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `len`.
        unsafe { args::write(len, 0, "len") }?;
        // SAFETY: eggdb.h asks for a live copy handle used by this call alone.
        let copy = unsafe { copy_handle(copy) }?;
        let (pointer, length) = match &mut copy.stage {
            Stage::LocalOut { data, read } => {
                let rest = data.get(*read..).unwrap_or_default();
                let piece = rest
                    .get(..rest.len().min(COPY_PIECE_BYTES))
                    .unwrap_or_default();
                *read = read.saturating_add(piece.len());
                if piece.is_empty() {
                    (std::ptr::null(), 0)
                } else {
                    (piece.as_ptr(), piece.len())
                }
            }
            Stage::RemoteOut => {
                // SAFETY: as in eggdb_copy_put.
                let state = unsafe { session_state(copy.session, "the copy's session") }?;
                copy.piece.clear();
                let remote = state.remote()?;
                let mut failure = None;
                while copy.piece.len() < COPY_PIECE_BYTES {
                    let next = remote.client().map(|client| client.next_copy_out_piece());
                    match next {
                        Ok(Ok(CopyOutPiece::Data(bytes))) => copy.piece.extend_from_slice(&bytes),
                        Ok(Ok(CopyOutPiece::Done(tag))) => {
                            publish_tag(&mut copy.results, tag);
                            copy.stage = Stage::RemoteOutDrained;
                            break;
                        }
                        Ok(Err(error)) => {
                            failure = Some(remote.failed(error));
                            break;
                        }
                        Err(broken) => {
                            failure = Some(broken);
                            break;
                        }
                    }
                }
                if let Some(failure) = failure {
                    copy.stage = Stage::Ended;
                    copy.piece.clear();
                    state.release(Holder::Copy(copy.id));
                    return Err(failure);
                }
                if copy.piece.is_empty() {
                    (std::ptr::null(), 0)
                } else {
                    (copy.piece.as_ptr(), copy.piece.len())
                }
            }
            Stage::RemoteOutDrained => (std::ptr::null(), 0),
            other => {
                return Err(wrong_stage(
                    other,
                    "eggdb_copy_read reads a copy from eggdb_sql_copy_out_begin before its end",
                ));
            }
        };
        // SAFETY: as for the first write to `data`.
        unsafe { args::write(data, pointer, "data") }?;
        // SAFETY: as for the first write to `len`.
        unsafe { args::write(len, length, "len") }
    })
}

fn end_copy(
    copy: &mut EggdbCopy,
    state: &mut crate::sql::SessionState,
) -> Result<(*const EggdbResult, usize), Failure> {
    let stage = std::mem::replace(&mut copy.stage, Stage::Ended);
    state.release(Holder::Copy(copy.id));
    copy.piece = Vec::new();
    match stage {
        Stage::LocalIn { text, data } => {
            let mut channel = CopyInData {
                remaining: &data,
                started: false,
            };
            let ran = state.run_copy_into(&mut copy.results, &text, &mut channel)?;
            if !channel.started {
                return Err(not_a_copy("COPY ... FROM STDIN"));
            }
            Ok(ran)
        }
        Stage::RemoteIn => finish_remote_copy_in(state.remote()?, &mut copy.results),
        Stage::LocalOut { .. } | Stage::RemoteOutDrained => Ok(copy.results.published()),
        Stage::RemoteOut => {
            let remote = state.remote()?;
            loop {
                let next = remote.client()?.next_copy_out_piece();
                match next.map_err(|error| remote.failed(error))? {
                    CopyOutPiece::Data(_) => {}
                    CopyOutPiece::Done(tag) => {
                        publish_tag(&mut copy.results, tag);
                        return Ok(copy.results.published());
                    }
                }
            }
        }
        Stage::Ended => Err(wrong_stage(&Stage::Ended, "free it with eggdb_copy_free")),
    }
}

/// Ends the copy and puts its result in `*results` (one, whose tag is `"COPY <rows>"`) and
/// their number in `*count`. A `COPY ... FROM STDIN` loads its rows here: every row or none,
/// with `EGGDB_ERR_SQL` and its SQLSTATE for a bad row. A `COPY ... TO STDOUT` not read to its
/// end is read to it and the rest of its data dropped. The results belong to the copy until
/// [`eggdb_copy_free`]. Whatever the status, the copy has ended and the session takes other
/// calls again; ending it twice fails with `EGGDB_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// `copy` is a handle from `eggdb_sql_copy_in_begin` or `eggdb_sql_copy_out_begin` used by
/// this call alone, with no other call on its session meanwhile (but `eggdb_sql_cancel`), and
/// `results` and `count` point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_copy_end(
    copy: *mut EggdbCopy,
    results: *mut *const EggdbResult,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `results`.
        unsafe { args::write(results, std::ptr::null(), "results") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live copy handle used by this call alone.
        let copy = unsafe { copy_handle(copy) }?;
        if matches!(copy.stage, Stage::Ended) {
            return Err(wrong_stage(&copy.stage, "free it with eggdb_copy_free"));
        }
        // SAFETY: as in eggdb_copy_put.
        let state = unsafe { session_state(copy.session, "the copy's session") }?;
        let (published, len) = end_copy(copy, state)?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

fn abandon(copy: &mut EggdbCopy, state: &mut crate::sql::SessionState) {
    let stage = std::mem::replace(&mut copy.stage, Stage::Ended);
    state.release(Holder::Copy(copy.id));
    let Ok(remote) = state.remote() else {
        return;
    };
    match stage {
        Stage::RemoteIn => {
            let failed = remote
                .client()
                .map(|client| client.fail_copy_in(FREED_BEFORE_ITS_END));
            if let Ok(Err(error)) = failed {
                let _ = remote.failed(error);
            }
        }
        Stage::RemoteOut => loop {
            let next = remote.client().map(|client| client.next_copy_out_piece());
            match next {
                Ok(Ok(CopyOutPiece::Data(_))) => {}
                Ok(Ok(CopyOutPiece::Done(_))) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = remote.failed(error);
                    return;
                }
            }
        },
        _ => {}
    }
}

/// Frees the copy and its results. A copy not ended is abandoned first: a `COPY ... FROM
/// STDIN` loads nothing (on a server it is failed with CopyFail), and the rest of a `COPY ...
/// TO STDOUT` is dropped. NULL does nothing.
///
/// # Safety
/// `copy` is NULL or a handle from `eggdb_sql_copy_in_begin` or `eggdb_sql_copy_out_begin` not
/// freed yet, with no other call on its session meanwhile; it and its results must not be used
/// afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_copy_free(copy: *mut EggdbCopy) {
    let _ = guarded(|| {
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(mut handle) = (unsafe { args::take(copy) }) else {
            return Ok(());
        };
        // SAFETY: a live copy's session is open, since it refuses to close while copies live,
        // and eggdb.h asks that no other call uses the session meanwhile.
        let state = unsafe { session_state(handle.session, "the copy's session") }?;
        abandon(&mut handle, state);
        // SAFETY: as above; this only counts the copy.
        unsafe { args::handle(handle.session, "the copy's session") }?.statement_finalized();
        Ok(())
    });
}

/// Runs the statements in `text` as `eggdb_sql_execute` does, with `len` bytes at `data` as
/// the data of the `COPY ... FROM STDIN` among them: text, CSV or binary, as the `COPY`'s
/// options say. The `COPY` loads all its rows or none, and its result's tag is `"COPY <rows>"`.
/// To load data that is not in one buffer, use [`eggdb_sql_copy_in_begin`]. A second `COPY
/// ... FROM STDIN` in `text`, and any `COPY ... TO STDOUT`, fail with `EGGDB_ERR_SQL`. On a
/// session from `eggdb_sql_connect`, `text` is the one `COPY ... FROM STDIN` and the data goes
/// to the server in 64 KiB pieces.
///
/// # Safety
/// As for `eggdb_sql_execute`; `data` is NULL with a length of 0 or points to `len` readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_copy_in(
    sql: *mut EggdbSql,
    text: *const c_char,
    data: *const u8,
    len: usize,
    results: *mut *const EggdbResult,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `results`.
        unsafe { args::write(results, std::ptr::null(), "results") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
        let state = unsafe { session_state(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?;
        // SAFETY: eggdb.h asks for NULL with 0 or `len` readable bytes at `data`.
        let data = unsafe { args::bytes(data, len, "data") }?;
        state.ensure_free()?;
        let (backend, batch) = state.parts();
        let (published, len) = match backend {
            Backend::Remote(remote) => copy_in_remote(remote, batch, text, data)?,
            Backend::MySql(_) => {
                return Err(Failure::invalid_argument(
                    "COPY is not available on a mysql:// session; use LOAD DATA or INSERT",
                ));
            }
            Backend::Local { .. } => {
                let mut channel = CopyInData {
                    remaining: data,
                    started: false,
                };
                let reader: &mut dyn CopyChannel = &mut channel;
                run_on(backend, batch, text, None, Some(reader))?
            }
        };
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

#[cfg(test)]
mod tests {
    use super::{COPY_PIECE_BYTES, CopyInData, GatheredCopyOut};
    use eggdb::sql::{CopyChannel, CopyFormat, CopyStart};

    fn start() -> CopyStart {
        CopyStart {
            format: CopyFormat::Text,
            columns: 2,
            file: None,
        }
    }

    #[test]
    fn copy_in_data_is_handed_over_in_bounded_pieces_to_one_copy() {
        let data = vec![b'x'; COPY_PIECE_BYTES * 2 + 5];
        let mut channel = CopyInData {
            remaining: &data,
            started: false,
        };
        assert!(channel.start_copy_in(&start()).is_ok());
        assert!(channel.start_copy_in(&start()).is_err());
        let mut sizes = Vec::new();
        let mut gathered = Vec::new();
        while channel.read_copy_data(&mut gathered).unwrap_or(false) {
            sizes.push(gathered.len());
        }
        assert_eq!(
            sizes,
            [
                COPY_PIECE_BYTES,
                COPY_PIECE_BYTES * 2,
                COPY_PIECE_BYTES * 2 + 5
            ]
        );
        assert!(channel.start_copy_out(&start()).is_err());
    }

    #[test]
    fn copy_out_data_is_gathered_from_one_copy() {
        let mut channel = GatheredCopyOut::default();
        assert!(channel.start_copy_out(&start()).is_ok());
        assert!(channel.write_copy_data(b"1\tAda\n").is_ok());
        assert!(channel.write_copy_data(b"2\tGrace\n").is_ok());
        assert!(channel.finish_copy_out().is_ok());
        assert_eq!(channel.data, b"1\tAda\n2\tGrace\n");
        assert!(channel.start_copy_out(&start()).is_err());
        assert!(channel.start_copy_in(&start()).is_err());
        let mut nothing = Vec::new();
        assert_eq!(channel.read_copy_data(&mut nothing).ok(), Some(false));
    }
}
