use super::{NAMESPACE, Reader, dialect_code, dialect_of, put_bytes, put_varint};
use crate::error::SqlResult;
use crate::triggers::{
    MAX_TRIGGER_TEXT, MAX_TRIGGERS_PER_TABLE, TableTriggers, TriggerAction, TriggerDefinition,
    TriggerEvents, TriggerLevel, TriggerTiming,
};

pub(super) const TRIGGER_SPACE: u8 = 0x0B;
const RECORD_VERSION: u8 = 1;
const ENABLED_FLAG: u8 = 1;
const CONDITION_FLAG: u8 = 2;
const OLD_TABLE_FLAG: u8 = 4;
const NEW_TABLE_FLAG: u8 = 8;
const KNOWN_FLAGS: u8 = ENABLED_FLAG | CONDITION_FLAG | OLD_TABLE_FLAG | NEW_TABLE_FLAG;
const STATEMENTS_ACTION: u8 = 1;
const BATCH_ACTION: u8 = 2;
const FUNCTION_ACTION: u8 = 3;
const MAX_LISTED: usize = 1600;

pub(crate) fn trigger_key(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(TRIGGER_SPACE);
    key.extend_from_slice(table.as_bytes());
    key
}

pub(crate) fn triggers_prefix() -> Vec<u8> {
    trigger_key("")
}

fn timing_code(timing: TriggerTiming) -> u8 {
    match timing {
        TriggerTiming::Before => 1,
        TriggerTiming::After => 2,
        TriggerTiming::InsteadOf => 3,
    }
}

fn timing_of(code: u8) -> Option<TriggerTiming> {
    match code {
        1 => Some(TriggerTiming::Before),
        2 => Some(TriggerTiming::After),
        3 => Some(TriggerTiming::InsteadOf),
        _ => None,
    }
}

fn level_code(level: TriggerLevel) -> u8 {
    match level {
        TriggerLevel::Row => 1,
        TriggerLevel::Statement => 2,
    }
}

fn level_of(code: u8) -> Option<TriggerLevel> {
    match code {
        1 => Some(TriggerLevel::Row),
        2 => Some(TriggerLevel::Statement),
        _ => None,
    }
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_bytes(out, text.as_bytes());
}

pub(crate) fn encode_triggers(triggers: &TableTriggers) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    put_varint(&mut out, triggers.triggers.len() as u64);
    for trigger in &triggers.triggers {
        encode_trigger(&mut out, trigger);
    }
    out
}

fn encode_trigger(out: &mut Vec<u8>, trigger: &TriggerDefinition) {
    put_text(out, &trigger.name);
    out.push(dialect_code(trigger.dialect));
    out.push(timing_code(trigger.timing));
    out.push(trigger.events.bits());
    out.push(level_code(trigger.level));
    let mut flags = 0;
    if trigger.enabled {
        flags |= ENABLED_FLAG;
    }
    if trigger.condition.is_some() {
        flags |= CONDITION_FLAG;
    }
    if trigger.old_table.is_some() {
        flags |= OLD_TABLE_FLAG;
    }
    if trigger.new_table.is_some() {
        flags |= NEW_TABLE_FLAG;
    }
    out.push(flags);
    put_varint(out, trigger.columns.len() as u64);
    for column in &trigger.columns {
        put_text(out, column);
    }
    for text in [&trigger.condition, &trigger.old_table, &trigger.new_table]
        .into_iter()
        .flatten()
    {
        put_text(out, text);
    }
    match &trigger.action {
        TriggerAction::Statements(text) => {
            out.push(STATEMENTS_ACTION);
            put_text(out, text);
        }
        TriggerAction::Batch(text) => {
            out.push(BATCH_ACTION);
            put_text(out, text);
        }
        TriggerAction::Function { name, arguments } => {
            out.push(FUNCTION_ACTION);
            put_text(out, name);
            put_varint(out, arguments.len() as u64);
            for argument in arguments {
                put_text(out, argument);
            }
        }
    }
    put_text(out, &trigger.text);
}

fn bounded_text(reader: &mut Reader<'_>) -> SqlResult<String> {
    let text = reader.text()?;
    if text.len() > MAX_TRIGGER_TEXT {
        return Err(reader.damaged());
    }
    Ok(text)
}

fn decode_trigger(reader: &mut Reader<'_>) -> SqlResult<TriggerDefinition> {
    let name = reader.plain_name()?;
    let dialect = dialect_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
    let timing = timing_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
    let events = TriggerEvents::from_bits(reader.byte()?).ok_or_else(|| reader.damaged())?;
    let level = level_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
    let flags = reader.byte()?;
    if flags & !KNOWN_FLAGS != 0 {
        return Err(reader.damaged());
    }
    let column_count = reader.count()?;
    if column_count > MAX_LISTED {
        return Err(reader.damaged());
    }
    let mut columns = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        columns.push(reader.plain_name()?);
    }
    let condition = if flags & CONDITION_FLAG != 0 {
        Some(bounded_text(reader)?)
    } else {
        None
    };
    let old_table = if flags & OLD_TABLE_FLAG != 0 {
        Some(reader.plain_name()?)
    } else {
        None
    };
    let new_table = if flags & NEW_TABLE_FLAG != 0 {
        Some(reader.plain_name()?)
    } else {
        None
    };
    let action = match reader.byte()? {
        STATEMENTS_ACTION => TriggerAction::Statements(bounded_text(reader)?),
        BATCH_ACTION => TriggerAction::Batch(bounded_text(reader)?),
        FUNCTION_ACTION => {
            let name = reader.name()?;
            let count = reader.count()?;
            if count > MAX_LISTED {
                return Err(reader.damaged());
            }
            let mut arguments = Vec::with_capacity(count);
            for _ in 0..count {
                arguments.push(bounded_text(reader)?);
            }
            TriggerAction::Function { name, arguments }
        }
        _ => return Err(reader.damaged()),
    };
    let text = bounded_text(reader)?;
    let trigger = TriggerDefinition {
        name,
        dialect,
        timing,
        events,
        columns,
        level,
        condition,
        old_table,
        new_table,
        action,
        enabled: flags & ENABLED_FLAG != 0,
        text,
    };
    if !trigger.is_consistent() {
        return Err(reader.damaged());
    }
    Ok(trigger)
}

pub(crate) fn decode_triggers(table: &str, bytes: &[u8]) -> SqlResult<TableTriggers> {
    let mut reader = Reader::new(bytes, "trigger definitions");
    reader.version()?;
    if table.is_empty() || !crate::schemas::is_well_formed(table) {
        return Err(reader.damaged());
    }
    let count = reader.count()?;
    if count == 0 || count > MAX_TRIGGERS_PER_TABLE {
        return Err(reader.damaged());
    }
    let mut triggers: Vec<TriggerDefinition> = Vec::with_capacity(count);
    for _ in 0..count {
        let trigger = decode_trigger(&mut reader)?;
        if triggers.iter().any(|known| known.name == trigger.name) {
            return Err(reader.damaged());
        }
        triggers.push(trigger);
    }
    reader.finish()?;
    Ok(TableTriggers {
        table: table.to_owned(),
        triggers,
    })
}
