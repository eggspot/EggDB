use super::{QueryResult, Session};
use crate::ast::CursorCommand;
use crate::copy::CopyIo;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::portals::Portal;

impl Session<'_> {
    pub(super) fn cursor_command(
        &mut self,
        command: &mut CursorCommand,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        match command {
            CursorCommand::Declare {
                name,
                scroll,
                hold,
                query,
            } => {
                if !*hold && !self.in_block() {
                    return Err(SqlError::new(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "DECLARE CURSOR can only be used in transaction blocks",
                    ));
                }
                if crate::portals::is_open(name) {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_CURSOR,
                        format!("cursor \"{name}\" already exists"),
                    ));
                }
                let result = self.run(query, io)?;
                crate::portals::declare(Portal {
                    name: name.clone(),
                    columns: result.columns.unwrap_or_default(),
                    rows: result.rows,
                    position: 0,
                    scroll: *scroll,
                    holdable: *hold,
                })?;
                Ok(QueryResult::command("DECLARE CURSOR"))
            }
            CursorCommand::Fetch {
                name,
                direction,
                only_move,
            } => crate::portals::fetch(name, *direction, *only_move),
            CursorCommand::Close(name) => crate::portals::close(name.as_deref()),
        }
    }
}
