use crate::connection::{
    Connection, LOGGED_BATCH_CHARACTERS, NOT_SUPPORTED, printable, report_wire_error,
};
use crate::rpc::{
    Procedure, ProcedureCall, RpcError, RpcParameter, SystemProcedure, parse_rpc_request,
};
use crate::tokens::{
    self, DONE_ERROR, DONE_IN_PROC, DONE_IN_TRANSACTION, DONE_MORE, DONE_PROC, ServerMessage,
};
use crate::wire::WireError;
use eggdb_sql::{
    KeyValueStore, NamedParameter, ParameterDeclaration, Session, SqlError, TransactionStatus,
    Value,
};
use std::collections::HashMap;
use std::sync::Arc;

const MAX_PREPARED_BATCHES: usize = 65_536;
const MAX_PREPARED_BYTES: usize = 64 * 1024 * 1024;
const OUT_OF_MEMORY: u32 = 701;
const TOO_MANY_PARAMETERS: u32 = 8003;
const UNKNOWN_PROCEDURE: u32 = 2812;
const WRONG_STATEMENT_TYPE: u32 = 214;
const MISSING_ARGUMENT: u32 = 201;
const TOO_MANY_ARGUMENTS: u32 = 8144;
const NOT_A_PARAMETER: u32 = 8145;
const NOT_DECLARED_OUTPUT: u32 = 8162;
const PARAMETER_NOT_SUPPLIED: u32 = 8178;
const UNKNOWN_HANDLE: u32 = 8179;
const INTN: u8 = 0x26;

pub(crate) struct PreparedBatch {
    text: String,
    declarations_text: String,
    declarations: Vec<ParameterDeclaration>,
}

#[derive(Default)]
pub(crate) struct PreparedBatches {
    batches: HashMap<i32, Arc<PreparedBatch>>,
    next_handle: i32,
    bytes: usize,
}

impl PreparedBatches {
    fn prepare(&mut self, batch: PreparedBatch) -> Result<i32, CallError> {
        let size = batch.text.len() + batch.declarations_text.len();
        if self.batches.len() >= MAX_PREPARED_BATCHES
            || self.bytes.saturating_add(size) > MAX_PREPARED_BYTES
        {
            return Err(CallError {
                number: OUT_OF_MEMORY,
                severity: 17,
                text: format!(
                    "There is insufficient memory to prepare another statement: EggDB keeps at most {MAX_PREPARED_BATCHES} prepared statements and {} MiB of their text per connection. Unprepare some first.",
                    MAX_PREPARED_BYTES / (1024 * 1024)
                ),
            });
        }
        let handle = loop {
            self.next_handle = self.next_handle.checked_add(1).unwrap_or(1);
            if !self.batches.contains_key(&self.next_handle) {
                break self.next_handle;
            }
        };
        self.bytes += size;
        self.batches.insert(handle, Arc::new(batch));
        Ok(handle)
    }

    fn get(&self, handle: i32) -> Option<Arc<PreparedBatch>> {
        self.batches.get(&handle).map(Arc::clone)
    }

    fn unprepare(&mut self, handle: i32) -> bool {
        match self.batches.remove(&handle) {
            Some(batch) => {
                self.bytes = self
                    .bytes
                    .saturating_sub(batch.text.len() + batch.declarations_text.len());
                true
            }
            None => false,
        }
    }
}

struct CallError {
    number: u32,
    severity: u8,
    text: String,
}

impl CallError {
    fn new(number: u32, text: String) -> Self {
        Self {
            number,
            severity: 16,
            text,
        }
    }

    fn from_sql(error: &SqlError) -> Self {
        Self {
            number: error.vendor_code().map_or(50_000, u32::from),
            severity: error.severity().unwrap_or(16),
            text: error.message().to_owned(),
        }
    }
}

#[derive(Default)]
struct Called {
    returned: Vec<Vec<u8>>,
    wrote_error: bool,
    failed_write: bool,
}

fn character_argument(parameter: Option<&RpcParameter>, name: &str) -> Result<String, CallError> {
    match parameter.map(|parameter| &parameter.value) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::Text(text)) => Ok(text.clone()),
        Some(_) => Err(CallError::new(
            WRONG_STATEMENT_TYPE,
            format!("Procedure expects parameter '{name}' of type 'ntext/nchar/nvarchar'."),
        )),
    }
}

fn handle_argument(
    parameter: Option<&RpcParameter>,
    procedure: SystemProcedure,
) -> Result<i32, CallError> {
    let missing = || {
        CallError::new(
            MISSING_ARGUMENT,
            format!(
                "Procedure or function '{}' expects parameter '@handle', which was not supplied.",
                procedure.name()
            ),
        )
    };
    let parameter = parameter.ok_or_else(missing)?;
    let handle = parameter
        .value
        .as_integer()
        .and_then(|handle| i32::try_from(handle).ok())
        .ok_or_else(missing)?;
    Ok(handle)
}

fn unknown_handle(handle: i32) -> CallError {
    CallError::new(
        UNKNOWN_HANDLE,
        format!("Could not find prepared statement with handle {handle}."),
    )
}

fn bound_parameters(
    procedure: SystemProcedure,
    batch: &PreparedBatch,
    arguments: &[RpcParameter],
) -> Result<Vec<NamedParameter>, CallError> {
    let mut values: Vec<Option<Value>> = vec![None; batch.declarations.len()];
    let mut position = 0;
    for argument in arguments {
        let index = if argument.name.is_empty() {
            let index = position;
            position += 1;
            if index >= batch.declarations.len() {
                return Err(CallError::new(
                    TOO_MANY_ARGUMENTS,
                    format!(
                        "Procedure or function {} has too many arguments specified.",
                        procedure.name()
                    ),
                ));
            }
            index
        } else {
            batch
                .declarations
                .iter()
                .position(|declaration| declaration.name.eq_ignore_ascii_case(&argument.name))
                .ok_or_else(|| {
                    CallError::new(
                        NOT_A_PARAMETER,
                        format!(
                            "{} is not a parameter for procedure {}.",
                            argument.name,
                            procedure.name()
                        ),
                    )
                })?
        };
        let declaration = batch.declarations.get(index);
        if argument.output && declaration.is_some_and(|declaration| !declaration.output) {
            return Err(CallError::new(
                NOT_DECLARED_OUTPUT,
                format!(
                    "The formal parameter \"{}\" was not declared as an OUTPUT parameter, but the actual parameter passed in requested output.",
                    declaration.map_or("", |declaration| declaration.name.as_str())
                ),
            ));
        }
        let value = if argument.uses_default {
            Value::Null
        } else {
            argument.value.clone()
        };
        if let Some(slot) = values.get_mut(index) {
            *slot = Some(value);
        }
    }
    batch
        .declarations
        .iter()
        .zip(values)
        .map(|(declaration, value)| match value {
            Some(value) => Ok(NamedParameter {
                name: declaration.name.clone(),
                declared_type: Some(declaration.sql_type),
                value,
            }),
            None => Err(CallError::new(
                PARAMETER_NOT_SUPPLIED,
                format!(
                    "The parameterized query '({}){}' expects the parameter '{}', which was not supplied.",
                    batch.declarations_text, batch.text, declaration.name
                ),
            )),
        })
        .collect()
}

fn returned_values(parameters: &[RpcParameter], first: usize) -> Vec<Vec<u8>> {
    parameters
        .iter()
        .enumerate()
        .skip(first)
        .filter_map(|(ordinal, parameter)| {
            let sent = parameter.sent.as_deref()?;
            let ordinal = u16::try_from(ordinal).ok()?;
            Some(tokens::return_value(ordinal, &parameter.name, sent))
        })
        .collect()
}

fn returned_handle(parameter: Option<&RpcParameter>, handle: i32) -> Vec<u8> {
    let mut type_and_value = vec![INTN, 4, 4];
    type_and_value.extend_from_slice(&handle.to_le_bytes());
    let name = parameter.map_or("", |parameter| parameter.name.as_str());
    tokens::return_value(0, name, &type_and_value)
}

fn catalog_procedure_name(procedure: &Procedure) -> Option<String> {
    let Procedure::Name(name) = procedure else {
        return None;
    };
    let bare = name
        .rsplit('.')
        .next()
        .unwrap_or(name)
        .trim_matches(|character| character == '[' || character == ']')
        .to_ascii_lowercase();
    eggdb_sql::is_catalog_procedure(&bare).then_some(bare)
}

fn is_parameter_name(name: &str) -> bool {
    name.strip_prefix('@').is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|character| character.is_alphanumeric() || "_@#$".contains(character))
    })
}

impl Connection<'_> {
    pub(crate) fn rpc<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        body: &[u8],
    ) -> bool {
        let request = match parse_rpc_request(body) {
            Ok(request) => request,
            Err(RpcError::Malformed(error)) => {
                report_wire_error(
                    self.shared,
                    &mut self.wire,
                    &WireError::Malformed(error.reason),
                );
                return false;
            }
            Err(RpcError::UnsupportedType(kind)) => {
                let text = format!(
                    "Parameters of TDS type 0x{kind:02X} (sql_variant, xml, user-defined and table-valued types) are not supported in this version of EggDB."
                );
                return self.send_error(NOT_SUPPORTED, 16, &text);
            }
            Err(RpcError::EncryptedParameter) => {
                return self.send_error(
                    NOT_SUPPORTED,
                    16,
                    "Encrypted parameters (Always Encrypted) are not supported in this version of EggDB.",
                );
            }
            Err(RpcError::TooManyParameters) => {
                return self.send_error(
                    TOO_MANY_PARAMETERS,
                    16,
                    "The incoming request has too many parameters. The server supports a maximum of 2100 parameters. Reduce the number of parameters and resend the request.",
                );
            }
        };
        if !self.begin_response() {
            return false;
        }
        let count = request.calls.len();
        for (index, call) in request.calls.iter().enumerate() {
            self.log_call(call);
            if !self.call(session, call, index + 1 < count) {
                return false;
            }
        }
        self.wire.end_message().is_ok()
    }

    fn log_call(&self, call: &ProcedureCall) {
        if !self.shared.config.log_batches {
            return;
        }
        let shown = match (call.procedure.system_procedure(), call.parameters.first()) {
            (
                Some(SystemProcedure::ExecuteSql),
                Some(RpcParameter {
                    value: Value::Text(text),
                    ..
                }),
            ) => text.chars().take(LOGGED_BATCH_CHARACTERS).collect(),
            (Some(SystemProcedure::Prepare | SystemProcedure::PrepareExecute), _) => {
                match call.parameters.get(2).map(|parameter| &parameter.value) {
                    Some(Value::Text(text)) => text.chars().take(LOGGED_BATCH_CHARACTERS).collect(),
                    _ => String::new(),
                }
            }
            (_, Some(first)) => format!("{:?}", first.value),
            (_, None) => String::new(),
        };
        (self.shared.log)(&format!(
            "{}  rpc {}: {}",
            self.wire.peer(),
            printable(&call.procedure.shown()),
            printable(&shown)
        ));
    }

    fn call<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        call: &ProcedureCall,
        more: bool,
    ) -> bool {
        let parameters = call.parameters.as_slice();
        let outcome = match call.procedure.system_procedure() {
            Some(SystemProcedure::ExecuteSql) => self.execute_sql(session, parameters),
            Some(SystemProcedure::Prepare) => self.prepare(session, parameters),
            Some(SystemProcedure::PrepareExecute) => self.prepare_and_execute(session, parameters),
            Some(SystemProcedure::Execute) => self.execute_prepared(session, parameters),
            Some(SystemProcedure::Unprepare) => self.unprepare(parameters),
            None => match catalog_procedure_name(&call.procedure) {
                Some(name) => self.catalog_procedure(session, &name, parameters),
                None => Err(CallError::new(
                    UNKNOWN_PROCEDURE,
                    format!(
                        "Could not find stored procedure '{}'.",
                        call.procedure.shown()
                    ),
                )),
            },
        };
        let in_transaction = if session.transaction_status() == TransactionStatus::Idle {
            0
        } else {
            DONE_IN_TRANSACTION
        };
        let more = if more { DONE_MORE } else { 0 };
        let called = match outcome {
            Ok(called) => called,
            Err(error) => {
                let mut tokens_out = tokens::error(&ServerMessage {
                    number: error.number,
                    severity: error.severity,
                    text: &error.text,
                    line: 1,
                });
                tokens_out.extend(tokens::done(
                    DONE_PROC,
                    DONE_ERROR | more | in_transaction,
                    0,
                    0,
                ));
                return self.wire.write(&tokens_out).is_ok();
            }
        };
        if called.failed_write {
            return false;
        }
        let mut tokens_out = tokens::return_status(0);
        for returned in &called.returned {
            tokens_out.extend_from_slice(returned);
        }
        let error = if called.wrote_error { DONE_ERROR } else { 0 };
        tokens_out.extend(tokens::done(DONE_PROC, error | more | in_transaction, 0, 0));
        self.wire.write(&tokens_out).is_ok()
    }

    fn run_bound<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        batch: &PreparedBatch,
        parameters: &[NamedParameter],
    ) -> Called {
        let ran = self.run_statements(session, &batch.text, Some(parameters), DONE_IN_PROC);
        let mut failed_write = ran.failed_write;
        if !failed_write && let Some(last) = ran.last {
            let token = tokens::done(
                DONE_IN_PROC,
                last.status | DONE_MORE,
                last.command,
                last.rows,
            );
            failed_write = self.wire.write(&token).is_err();
        }
        Called {
            returned: Vec::new(),
            wrote_error: ran.wrote_error,
            failed_write,
        }
    }

    fn checked_batch<S: KeyValueStore>(
        session: &mut Session<'_, S>,
        declarations_text: String,
        text: String,
    ) -> Result<PreparedBatch, CallError> {
        let declarations = session
            .parse_parameter_declarations(&declarations_text)
            .map_err(|error| CallError::from_sql(&error))?;
        let placeholders: Vec<NamedParameter> = declarations
            .iter()
            .map(|declaration| NamedParameter {
                name: declaration.name.clone(),
                declared_type: Some(declaration.sql_type),
                value: Value::Null,
            })
            .collect();
        session
            .check_parameterized_batch(&text, &placeholders)
            .map_err(|error| CallError::from_sql(&error))?;
        Ok(PreparedBatch {
            text,
            declarations_text,
            declarations,
        })
    }

    fn execute_sql<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        parameters: &[RpcParameter],
    ) -> Result<Called, CallError> {
        let text = character_argument(parameters.first(), "@statement")?;
        let declarations_text = character_argument(parameters.get(1), "@params")?;
        let declarations = session
            .parse_parameter_declarations(&declarations_text)
            .map_err(|error| CallError::from_sql(&error))?;
        let batch = PreparedBatch {
            text,
            declarations_text,
            declarations,
        };
        let bound = bound_parameters(
            SystemProcedure::ExecuteSql,
            &batch,
            parameters.get(2..).unwrap_or_default(),
        )?;
        let mut called = self.run_bound(session, &batch, &bound);
        called.returned = returned_values(parameters, 2);
        Ok(called)
    }

    fn prepare<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        parameters: &[RpcParameter],
    ) -> Result<Called, CallError> {
        let declarations_text = character_argument(parameters.get(1), "@params")?;
        let text = character_argument(parameters.get(2), "@stmt")?;
        let batch = Self::checked_batch(session, declarations_text, text)?;
        let handle = self.prepared.prepare(batch)?;
        Ok(Called {
            returned: vec![returned_handle(parameters.first(), handle)],
            ..Called::default()
        })
    }

    fn prepare_and_execute<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        parameters: &[RpcParameter],
    ) -> Result<Called, CallError> {
        if let Some(old) = parameters
            .first()
            .and_then(|parameter| parameter.value.as_integer())
            .and_then(|handle| i32::try_from(handle).ok())
            .filter(|&handle| handle > 0)
        {
            self.prepared.unprepare(old);
        }
        let declarations_text = character_argument(parameters.get(1), "@params")?;
        let text = character_argument(parameters.get(2), "@stmt")?;
        let batch = Self::checked_batch(session, declarations_text, text)?;
        let bound = bound_parameters(
            SystemProcedure::PrepareExecute,
            &batch,
            parameters.get(3..).unwrap_or_default(),
        )?;
        let handle = self.prepared.prepare(batch)?;
        let batch = self
            .prepared
            .get(handle)
            .ok_or_else(|| unknown_handle(handle))?;
        let mut called = self.run_bound(session, &batch, &bound);
        called.returned = vec![returned_handle(parameters.first(), handle)];
        called.returned.extend(returned_values(parameters, 3));
        Ok(called)
    }

    fn execute_prepared<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        parameters: &[RpcParameter],
    ) -> Result<Called, CallError> {
        let handle = handle_argument(parameters.first(), SystemProcedure::Execute)?;
        let batch = self
            .prepared
            .get(handle)
            .ok_or_else(|| unknown_handle(handle))?;
        let bound = bound_parameters(
            SystemProcedure::Execute,
            &batch,
            parameters.get(1..).unwrap_or_default(),
        )?;
        let mut called = self.run_bound(session, &batch, &bound);
        called.returned = returned_values(parameters, 1);
        Ok(called)
    }

    fn catalog_procedure<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        name: &str,
        parameters: &[RpcParameter],
    ) -> Result<Called, CallError> {
        let mut text = format!("EXEC {name}");
        let mut bound: Vec<NamedParameter> = Vec::with_capacity(parameters.len());
        for (index, parameter) in parameters.iter().enumerate() {
            text.push_str(if index == 0 { " " } else { ", " });
            if !parameter.name.is_empty() {
                if !is_parameter_name(&parameter.name) {
                    return Err(CallError::new(
                        NOT_A_PARAMETER,
                        format!(
                            "{} is not a parameter for procedure {name}.",
                            printable(&parameter.name)
                        ),
                    ));
                }
                text.push_str(&parameter.name);
                text.push_str(" = ");
            }
            if parameter.uses_default {
                text.push_str("DEFAULT");
                continue;
            }
            let placeholder = format!("@eggdb_rpc_argument_{index}");
            text.push_str(&placeholder);
            bound.push(NamedParameter {
                name: placeholder,
                declared_type: None,
                value: parameter.value.clone(),
            });
        }
        let batch = PreparedBatch {
            text,
            declarations_text: String::new(),
            declarations: Vec::new(),
        };
        Ok(self.run_bound(session, &batch, &bound))
    }

    fn unprepare(&mut self, parameters: &[RpcParameter]) -> Result<Called, CallError> {
        let handle = handle_argument(parameters.first(), SystemProcedure::Unprepare)?;
        if !self.prepared.unprepare(handle) {
            return Err(unknown_handle(handle));
        }
        Ok(Called::default())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedBatch, PreparedBatches, bound_parameters, catalog_procedure_name, is_parameter_name,
    };
    use crate::rpc::{Procedure, RpcParameter, SystemProcedure};
    use eggdb_sql::{ParameterDeclaration, SqlType, Value};

    fn batch() -> PreparedBatch {
        PreparedBatch {
            text: "SELECT @a, @b".into(),
            declarations_text: "@a int, @b nvarchar(10) OUTPUT".into(),
            declarations: vec![
                ParameterDeclaration {
                    name: "@a".into(),
                    sql_type: SqlType::Integer,
                    output: false,
                },
                ParameterDeclaration {
                    name: "@b".into(),
                    sql_type: SqlType::Varchar(Some(10)),
                    output: true,
                },
            ],
        }
    }

    fn argument(name: &str, output: bool, value: Value) -> RpcParameter {
        RpcParameter {
            name: name.into(),
            output,
            uses_default: false,
            value,
            sent: None,
        }
    }

    fn number(result: Result<Vec<eggdb_sql::NamedParameter>, super::CallError>) -> Option<u32> {
        result.err().map(|error| error.number)
    }

    #[test]
    fn arguments_bind_by_position_or_by_name_to_the_declarations() {
        let procedure = SystemProcedure::ExecuteSql;
        let by_position = bound_parameters(
            procedure,
            &batch(),
            &[
                argument("", false, Value::Integer(1)),
                argument("", true, Value::Text("x".into())),
            ],
        )
        .ok()
        .unwrap();
        assert_eq!(by_position[0].name, "@a");
        assert_eq!(by_position[0].declared_type, Some(SqlType::Integer));
        assert_eq!(by_position[1].value, Value::Text("x".into()));
        let by_name = bound_parameters(
            procedure,
            &batch(),
            &[
                argument("@B", false, Value::Null),
                argument("@A", false, Value::Integer(2)),
            ],
        )
        .ok()
        .unwrap();
        assert_eq!(by_name[0].value, Value::Integer(2));
        assert_eq!(
            number(bound_parameters(
                procedure,
                &batch(),
                &[argument("", false, Value::Integer(1))]
            )),
            Some(8178)
        );
        assert_eq!(
            number(bound_parameters(
                procedure,
                &batch(),
                &[
                    argument("", false, Value::Null),
                    argument("", false, Value::Null),
                    argument("", false, Value::Null)
                ]
            )),
            Some(8144)
        );
        assert_eq!(
            number(bound_parameters(
                procedure,
                &batch(),
                &[argument("@c", false, Value::Null)]
            )),
            Some(8145)
        );
        assert_eq!(
            number(bound_parameters(
                procedure,
                &batch(),
                &[argument("@a", true, Value::Null)]
            )),
            Some(8162)
        );
    }

    #[test]
    fn prepared_handles_count_up_and_are_forgotten_on_unprepare() {
        let mut prepared = PreparedBatches::default();
        let first = prepared.prepare(batch()).ok().unwrap();
        let second = prepared.prepare(batch()).ok().unwrap();
        assert_eq!((first, second), (1, 2));
        assert!(prepared.get(first).is_some());
        assert!(prepared.unprepare(first));
        assert!(!prepared.unprepare(first));
        assert!(prepared.get(first).is_none());
        assert_eq!(
            prepared.bytes,
            batch().text.len() + batch().declarations_text.len()
        );
    }

    #[test]
    fn catalog_procedures_are_called_by_their_last_name_part_in_any_case() {
        for (sent, expected) in [
            ("sp_tables", Some("sp_tables")),
            ("[sys].[SP_COLUMNS_100]", Some("sp_columns_100")),
            ("master.dbo.sp_pkeys", Some("sp_pkeys")),
            ("sp_nothing", None),
        ] {
            assert_eq!(
                catalog_procedure_name(&Procedure::Name(sent.into())).as_deref(),
                expected,
                "{sent}"
            );
        }
        assert_eq!(catalog_procedure_name(&Procedure::Id(10)), None);
    }

    #[test]
    fn only_plain_parameter_names_reach_the_generated_exec_text() {
        assert!(is_parameter_name("@table_name"));
        assert!(is_parameter_name("@ODBCVer"));
        assert!(!is_parameter_name("table_name"));
        assert!(!is_parameter_name("@"));
        assert!(!is_parameter_name("@a = 1; DROP TABLE t; --"));
        assert!(!is_parameter_name("@a'"));
    }
}
