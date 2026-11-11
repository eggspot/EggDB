use crate::connection::MAX_PACKET_BYTES;
use crate::wire::Wire;
use eggdb_sql::{CopyChannel, CopyStart, SqlError, SqlResult, SqlState};

const LOCAL_INFILE_REQUEST: u8 = 0xFB;

/// The client end of `LOAD DATA LOCAL INFILE`: asks the client for the file the statement names
/// and reads its content, packet by packet, until the empty packet that ends it.
pub(crate) struct LocalInfile<'w, 'a> {
    pub(crate) wire: &'w mut Wire<'a>,
    pub(crate) allowed: bool,
    started: bool,
    ended: bool,
}

impl<'w, 'a> LocalInfile<'w, 'a> {
    pub(crate) fn new(wire: &'w mut Wire<'a>, allowed: bool) -> Self {
        Self {
            wire,
            allowed,
            started: false,
            ended: false,
        }
    }
}

fn lost() -> SqlError {
    SqlError::new(
        SqlState::MYSQL_GENERAL,
        "Lost connection to MySQL client while reading the local file",
    )
    .with_vendor_code(2013)
}

fn refused(what: &str) -> SqlError {
    SqlError::not_supported(format!("{what} over the MySQL protocol"))
}

impl CopyChannel for LocalInfile<'_, '_> {
    fn start_copy_in(&mut self, start: &CopyStart) -> SqlResult<()> {
        let Some(file) = &start.file else {
            return Err(refused("COPY ... FROM STDIN"));
        };
        if !self.allowed {
            return Err(SqlError::new(
                SqlState::MYSQL_GENERAL,
                "Loading local data is disabled; this must be enabled on both the client and server sides",
            )
            .with_vendor_code(3948));
        }
        let mut payload = Vec::with_capacity(file.len() + 1);
        payload.push(LOCAL_INFILE_REQUEST);
        payload.extend_from_slice(file.as_bytes());
        self.wire.send(&payload);
        self.wire.flush().map_err(|_| lost())?;
        self.started = true;
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if !self.started || self.ended {
            return Ok(false);
        }
        let packet = self
            .wire
            .read_payload(self.wire.sequence, MAX_PACKET_BYTES)
            .map_err(|_| lost())?;
        self.wire.release_payload_reservation();
        if packet.is_empty() {
            self.ended = true;
            return Ok(false);
        }
        data.extend_from_slice(&packet);
        Ok(true)
    }

    fn abandon_copy_in(&mut self) {
        let mut ignored = Vec::new();
        while self.read_copy_data(&mut ignored).unwrap_or(false) {
            ignored.clear();
        }
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(refused("COPY ... TO STDOUT"))
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Err(refused("COPY ... TO STDOUT"))
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Err(refused("COPY ... TO STDOUT"))
    }
}
