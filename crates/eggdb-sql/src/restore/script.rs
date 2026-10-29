use crate::error::{SqlError, SqlResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DifferenceNote {
    pub(crate) object: String,
    pub(crate) difference: String,
}

impl DifferenceNote {
    pub(crate) fn new(object: impl Into<String>, difference: impl Into<String>) -> Self {
        Self {
            object: object.into(),
            difference: difference.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptStatement {
    pub(crate) text: String,
    pub(crate) line: u64,
    pub(crate) end_line: u64,
    pub(crate) script: u32,
    pub(crate) copy_from_stdin: bool,
    pub(crate) refused: Option<SqlError>,
    pub(crate) differences: Vec<DifferenceNote>,
}

impl ScriptStatement {
    pub(crate) fn plain(text: String, line: u64, end_line: u64) -> Self {
        Self {
            text,
            line,
            end_line,
            script: 0,
            copy_from_stdin: false,
            refused: None,
            differences: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientCommand {
    pub(crate) name: String,
    pub(crate) shown: String,
    pub(crate) argument: String,
    pub(crate) line: u64,
    pub(crate) script: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ScriptItem {
    Statement(ScriptStatement),
    ClientCommand(ClientCommand),
    End,
}

pub(crate) trait ScriptSource {
    fn next_item(&mut self) -> SqlResult<ScriptItem>;

    fn line_number(&self) -> u64;

    fn skip_data(&mut self) -> SqlResult<u64>;

    fn copy_data(&mut self, out: &mut Vec<u8>) -> SqlResult<bool>;

    fn copy_lines(&self) -> u64;
}
