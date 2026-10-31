use crate::routines::RoutineDefinition;
use crate::sqlserver_types::SqlServerType;
use crate::types::{SqlType, Value};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TsqlRoutine {
    pub(crate) returns: TsqlReturns,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TsqlReturns {
    Status,
    Scalar(SqlServerType),
    InlineTable,
    Table {
        variable: String,
        columns: Vec<(String, SqlType)>,
        rules: crate::table_variables::TableVariableRules,
    },
}

impl TsqlReturns {
    pub(crate) fn returns_value(&self) -> bool {
        matches!(self, Self::Status | Self::Scalar(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcedureRun {
    pub(crate) definition: Arc<RoutineDefinition>,
    pub(crate) arguments: Vec<(String, Value)>,
    pub(crate) outputs: Vec<(String, String)>,
    pub(crate) status_variable: Option<String>,
}
