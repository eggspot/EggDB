use crate::advisory_locks::{Lasts, Mode};
use crate::ast::{Expr, ProcedureCall};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{QueryResult, Session};
use crate::types::Value;
use std::time::Duration;

const CALL_FAILED: i32 = -999;
const CANCELED: i32 = -2;
const NOT_HELD: u16 = 1223;
const INVALID_RESOURCE: u16 = 1224;
const INVALID_MODE: u16 = 1225;
const INVALID_OWNER: u16 = 1226;
const INVALID_TIMEOUT: u16 = 1227;
const INVALID_PARAMETER: u16 = 1228;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum ApplicationLockProcedure {
    Take,
    Release,
}

impl ApplicationLockProcedure {
    pub(in crate::session) fn named(name: &str) -> Option<Self> {
        match name {
            "sp_getapplock" => Some(Self::Take),
            "sp_releaseapplock" => Some(Self::Release),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Take => "sp_getapplock",
            Self::Release => "sp_releaseapplock",
        }
    }

    fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Take => &[
                "@resource",
                "@lockmode",
                "@lockowner",
                "@locktimeout",
                "@dbprincipal",
            ],
            Self::Release => &["@resource", "@lockowner", "@dbprincipal"],
        }
    }
}

struct LockRequest {
    resource: String,
    mode: Option<Mode>,
    lasts: Lasts,
    timeout: Option<Duration>,
    principal: String,
}

fn lock_error(code: u16, message: String) -> SqlError {
    crate::advisory_locks::application_lock_error(code, message)
}

fn text_of(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(value) => crate::sqlserver_conversion::text_of(value, None, None).ok(),
    }
}

fn request_of(
    procedure: ApplicationLockProcedure,
    values: &[Option<Value>],
) -> SqlResult<LockRequest> {
    let name = procedure.name();
    let value = |parameter: &str| {
        procedure
            .parameters()
            .iter()
            .position(|known| *known == parameter)
            .and_then(|at| values.get(at))
            .and_then(Option::as_ref)
    };
    let Some(resource) = text_of(value("@resource")).filter(|text| !text.is_empty()) else {
        return Err(lock_error(
            INVALID_RESOURCE,
            format!("An invalid application lock resource was passed to {name}."),
        ));
    };
    let mode = match procedure {
        ApplicationLockProcedure::Release => None,
        ApplicationLockProcedure::Take => {
            match text_of(value("@lockmode")).and_then(|text| Mode::named(&text)) {
                Some(mode) => Some(mode),
                None => {
                    return Err(lock_error(
                        INVALID_MODE,
                        format!("An invalid application lock mode was passed to {name}."),
                    ));
                }
            }
        }
    };
    let lasts = match text_of(value("@lockowner")) {
        None => Lasts::Transaction,
        Some(text) => Lasts::named(&text).ok_or_else(|| {
            lock_error(
                INVALID_OWNER,
                format!("An invalid application lock owner was passed to {name}."),
            )
        })?,
    };
    let timeout = match value("@locktimeout") {
        None | Some(Value::Null) => None,
        Some(given) => match given.as_integer() {
            Some(-1) => None,
            Some(milliseconds) => Some(Duration::from_millis(
                u64::try_from(milliseconds).map_err(|_| {
                    lock_error(
                        INVALID_TIMEOUT,
                        format!("An invalid application lock time-out was passed to {name}."),
                    )
                })?,
            )),
            None => {
                return Err(lock_error(
                    INVALID_TIMEOUT,
                    format!("An invalid application lock time-out was passed to {name}."),
                ));
            }
        },
    };
    let principal = text_of(value("@dbprincipal")).unwrap_or_else(|| "public".to_owned());
    Ok(LockRequest {
        resource: crate::advisory_locks::application_resource(&resource),
        mode,
        lasts,
        timeout,
        principal,
    })
}

impl Session<'_> {
    fn application_lock_arguments(
        &mut self,
        procedure: ApplicationLockProcedure,
        call: &ProcedureCall,
    ) -> SqlResult<Vec<Option<Value>>> {
        let parameters = procedure.parameters();
        let mut chosen: Vec<Option<Expr>> = vec![None; parameters.len()];
        for (ordinal, argument) in call.arguments.iter().enumerate() {
            let index = match &argument.name {
                None => Some(ordinal),
                Some(name) => {
                    let name = name.trim_start_matches('@');
                    parameters
                        .iter()
                        .position(|known| known.trim_start_matches('@').eq_ignore_ascii_case(name))
                }
            };
            let Some(slot) = index.and_then(|index| chosen.get_mut(index)) else {
                return Err(lock_error(
                    INVALID_PARAMETER,
                    format!(
                        "An invalid parameter \"{}\" was passed to the application lock function or procedure.",
                        argument.name.as_deref().unwrap_or("")
                    ),
                ));
            };
            *slot = argument.value.clone();
        }
        let given: Vec<Expr> = chosen.iter().flatten().cloned().collect();
        let mut values = if given.is_empty() {
            Vec::new()
        } else {
            self.evaluate_row(given)?
        }
        .into_iter();
        Ok(chosen
            .iter()
            .map(|expr| expr.as_ref().and_then(|_| values.next()))
            .collect())
    }

    fn application_lock_status(
        &mut self,
        procedure: ApplicationLockProcedure,
        call: &ProcedureCall,
    ) -> SqlResult<i32> {
        let values = self.application_lock_arguments(procedure, call)?;
        let request = request_of(procedure, &values)?;
        self.sync_transaction_count();
        let in_transaction = self.settings.sqlserver.transaction_count > 0;
        match (procedure, request.mode) {
            (ApplicationLockProcedure::Take, Some(mode)) => {
                if request.lasts == Lasts::Transaction && !in_transaction {
                    return Err(SqlError::new(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "You attempted to acquire a transactional application lock without an active transaction.",
                    ));
                }
                crate::advisory_locks::take_application_lock(
                    &request.resource,
                    mode,
                    request.lasts,
                    request.timeout,
                )
                .map(crate::advisory_locks::ApplicationLockGrant::status)
            }
            _ => {
                if crate::advisory_locks::release_application_lock(
                    &request.resource,
                    request.lasts,
                )? {
                    Ok(0)
                } else {
                    Err(lock_error(
                        NOT_HELD,
                        format!(
                            "Cannot release the application lock (Database Principal: '{}', Resource: '{}') because it is not currently held.",
                            request.principal, request.resource
                        ),
                    ))
                }
            }
        }
    }

    pub(in crate::session) fn run_application_lock_procedure(
        &mut self,
        procedure: ApplicationLockProcedure,
        call: &ProcedureCall,
    ) -> SqlResult<QueryResult> {
        let position = call.procedure.position;
        let outcome = self.application_lock_status(procedure, call);
        let status = match &outcome {
            Ok(status) => *status,
            Err(error) if error.state() == SqlState::QUERY_CANCELED => CANCELED,
            Err(_) => CALL_FAILED,
        };
        if let Some(variable) = &call.status_variable {
            self.assign_variable(&variable.text, Value::Integer(status))
                .map_err(|error| error.at_if_unplaced(variable.position))?;
        }
        outcome
            .map(|_| QueryResult::command("EXECUTE"))
            .map_err(|error| error.at_if_unplaced(position))
    }
}
