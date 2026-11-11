use crate::ast::FunctionBody;
use crate::interpreter::{Frame, Native};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A Lua value.
#[derive(Clone)]
pub enum Value {
    /// `nil`.
    Nil,
    /// `true` or `false`.
    Boolean(bool),
    /// A number (a double, as in Lua 5.1).
    Number(f64),
    /// A byte string.
    String(Rc<[u8]>),
    /// A table, shared by reference.
    Table(TableRef),
    /// A function.
    Function(Function),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Nil => formatter.write_str("nil"),
            Value::Boolean(flag) => write!(formatter, "{flag}"),
            Value::Number(number) => write!(formatter, "{}", number_text(*number)),
            Value::String(bytes) => write!(formatter, "{:?}", String::from_utf8_lossy(bytes)),
            Value::Table(_) => formatter.write_str("table"),
            Value::Function(_) => formatter.write_str("function"),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Boolean(left), Value::Boolean(right)) => left == right,
            (Value::Number(left), Value::Number(right)) => left == right,
            (Value::String(left), Value::String(right)) => left == right,
            (Value::Table(left), Value::Table(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Value::Function(left), Value::Function(right)) => left.same(right),
            _ => false,
        }
    }
}

impl Value {
    /// A string value of `text`.
    pub fn string(text: impl AsRef<[u8]>) -> Self {
        Value::String(text.as_ref().into())
    }

    /// Whether the value counts as true: everything but `nil` and `false`.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Boolean(false))
    }

    /// The value's type name, as `type()` gives it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Boolean(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Table(_) => "table",
            Value::Function(_) => "function",
        }
    }

    /// The number the value is, or converts to as Lua converts strings in arithmetic.
    pub fn to_number(&self) -> Option<f64> {
        match self {
            Value::Number(number) => Some(*number),
            Value::String(bytes) => parse_number(bytes),
            _ => None,
        }
    }

    /// The bytes of a string or number, as concatenation and the string library take them.
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Value::String(bytes) => Some(bytes.to_vec()),
            Value::Number(number) => Some(number_text(*number).into_bytes()),
            _ => None,
        }
    }
}

/// Parses `bytes` as a Lua number: decimal with an optional exponent, or `0x` hexadecimal,
/// with surrounding spaces.
pub fn parse_number(bytes: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let value = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()? as f64
    } else {
        if body.is_empty() || body.contains(|character: char| character.is_alphabetic() && !matches!(character, 'e' | 'E')) {
            return None;
        }
        body.parse::<f64>().ok()?
    };
    Some(if negative { -value } else { value })
}

/// The text of a number as Lua 5.1 prints it (`%.14g`).
pub fn number_text(number: f64) -> String {
    if number.is_nan() {
        return if number.is_sign_negative() { "-nan".to_owned() } else { "nan".to_owned() };
    }
    if number.is_infinite() {
        return if number > 0.0 { "inf".to_owned() } else { "-inf".to_owned() };
    }
    if number == 0.0 {
        return if number.is_sign_negative() { "-0".to_owned() } else { "0".to_owned() };
    }
    if number.fract() == 0.0 && number.abs() < 1e15 {
        return format!("{}", number as i64);
    }
    let exponent = number.abs().log10().floor() as i32;
    if !(-4..14).contains(&exponent) {
        let scientific = format!("{number:.13e}");
        let (mantissa, power) = scientific.split_once('e').unwrap_or((&scientific, "0"));
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        let power: i32 = power.parse().unwrap_or(0);
        let sign = if power < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", power.abs());
    }
    let decimals = usize::try_from(13 - exponent).unwrap_or(0);
    let fixed = format!("{number:.decimals$}");
    if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        fixed
    }
}

/// A function: a Lua closure or one of the interpreter's native functions.
#[derive(Clone)]
pub enum Function {
    /// A Lua function with the frame it closes over.
    Lua(Rc<Closure>),
    /// A native function and its name.
    Native(Native, &'static str),
}

impl Function {
    fn same(&self, other: &Function) -> bool {
        match (self, other) {
            (Function::Lua(left), Function::Lua(right)) => Rc::ptr_eq(left, right),
            (Function::Native(left, left_name), Function::Native(right, right_name)) => {
                std::ptr::fn_addr_eq(*left, *right) && left_name == right_name
            }
            _ => false,
        }
    }
}

/// A Lua function closed over the frame it was made in.
pub struct Closure {
    pub(crate) body: Rc<FunctionBody>,
    pub(crate) frame: Rc<Frame>,
}

/// A table shared by reference, compared by identity.
#[derive(Clone)]
pub struct TableRef(pub(crate) Rc<RefCell<Table>>);

impl TableRef {
    /// A new empty table.
    pub fn new() -> Self {
        TableRef(Rc::new(RefCell::new(Table::default())))
    }

    /// The value at `key`, without metamethods.
    pub fn get(&self, key: &Value) -> Value {
        self.0.borrow().get(key)
    }

    /// Sets `key` to `value`, without metamethods; `nil` or NaN keys are refused.
    pub fn set(&self, key: Value, value: Value) -> Result<(), String> {
        self.0.borrow_mut().set(key, value)
    }

    /// The length `#` gives: a border of the array part.
    pub fn length(&self) -> usize {
        self.0.borrow().length()
    }

    pub(crate) fn pairs(&self) -> Vec<(Value, Value)> {
        let table = self.0.borrow();
        let mut pairs = Vec::new();
        let mut key = Value::Nil;
        while let Ok(Some((next_key, value))) = table.next(&key) {
            pairs.push((next_key.clone(), value));
            key = next_key;
        }
        pairs
    }

    pub(crate) fn identity(&self) -> usize {
        Rc::as_ptr(&self.0) as *const u8 as usize
    }
}

impl Default for TableRef {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Boolean(bool),
    Number(u64),
    String(Rc<[u8]>),
    Reference(usize),
}

impl Key {
    fn of(value: &Value) -> Option<Key> {
        Some(match value {
            Value::Nil => return None,
            Value::Boolean(flag) => Key::Boolean(*flag),
            Value::Number(number) if number.is_nan() => return None,
            Value::Number(number) => Key::Number((*number + 0.0).to_bits()),
            Value::String(bytes) => Key::String(bytes.clone()),
            Value::Table(table) => Key::Reference(table.identity()),
            Value::Function(Function::Lua(closure)) => Key::Reference(Rc::as_ptr(closure) as *const u8 as usize),
            Value::Function(Function::Native(native, _)) => Key::Reference(*native as usize),
        })
    }
}

/// A Lua table: an array part for the keys 1..n and an ordered hash part for the rest.
#[derive(Default)]
pub struct Table {
    pub(crate) array: Vec<Value>,
    pub(crate) entries: Vec<(Value, Value)>,
    pub(crate) index: HashMap<Key, usize>,
    pub(crate) metatable: Option<TableRef>,
}

fn array_slot(key: &Value) -> Option<usize> {
    match key {
        Value::Number(number) if number.fract() == 0.0 && *number >= 1.0 && *number <= usize::MAX as f64 => {
            Some(*number as usize - 1)
        }
        _ => None,
    }
}

impl Table {
    pub(crate) fn get(&self, key: &Value) -> Value {
        if let Some(slot) = array_slot(key)
            && let Some(value) = self.array.get(slot)
        {
            return value.clone();
        }
        Key::of(key)
            .and_then(|key| self.index.get(&key))
            .map_or(Value::Nil, |at| self.entries[*at].1.clone())
    }

    pub(crate) fn set(&mut self, key: Value, value: Value) -> Result<(), String> {
        if let Some(slot) = array_slot(&key) {
            if slot < self.array.len() {
                self.array[slot] = value;
                while matches!(self.array.last(), Some(Value::Nil)) {
                    self.array.pop();
                }
                return Ok(());
            }
            if slot == self.array.len() && !matches!(value, Value::Nil) {
                self.array.push(value);
                if let Some(key) = Key::of(&key)
                    && let Some(at) = self.index.get(&key).copied()
                {
                    self.entries[at].1 = Value::Nil;
                }
                loop {
                    let next = Value::Number((self.array.len() + 1) as f64);
                    let Some(at) = Key::of(&next).and_then(|key| self.index.get(&key).copied()) else {
                        break;
                    };
                    let moved = std::mem::replace(&mut self.entries[at].1, Value::Nil);
                    if matches!(moved, Value::Nil) {
                        break;
                    }
                    self.array.push(moved);
                }
                return Ok(());
            }
        }
        let Some(hashed) = Key::of(&key) else {
            return Err(match key {
                Value::Nil => "table index is nil".to_owned(),
                _ => "table index is NaN".to_owned(),
            });
        };
        match self.index.get(&hashed) {
            Some(at) => self.entries[*at].1 = value,
            None if matches!(value, Value::Nil) => {}
            None => {
                self.index.insert(hashed, self.entries.len());
                self.entries.push((key, value));
            }
        }
        Ok(())
    }

    pub(crate) fn length(&self) -> usize {
        if !self.array.is_empty() {
            return self.array.len();
        }
        let mut border = 0;
        while !matches!(self.get(&Value::Number((border + 1) as f64)), Value::Nil) {
            border += 1;
        }
        border
    }

    /// The key and value after `key` in traversal order (the array part, then the hash part);
    /// `None` at the end; an error for a key not in the table.
    pub(crate) fn next(&self, key: &Value) -> Result<Option<(Value, Value)>, String> {
        let mut array_from = 0;
        let mut entries_from = 0;
        match key {
            Value::Nil => {}
            other => match array_slot(other).filter(|slot| *slot < self.array.len()) {
                Some(slot) => array_from = slot + 1,
                None => {
                    let at = Key::of(other)
                        .and_then(|hashed| self.index.get(&hashed).copied())
                        .ok_or_else(|| "invalid key to 'next'".to_owned())?;
                    array_from = self.array.len();
                    entries_from = at + 1;
                }
            },
        }
        for slot in array_from..self.array.len() {
            if !matches!(self.array[slot], Value::Nil) {
                return Ok(Some((Value::Number((slot + 1) as f64), self.array[slot].clone())));
            }
        }
        for (entry_key, entry_value) in &self.entries[entries_from.min(self.entries.len())..] {
            if !matches!(entry_value, Value::Nil) {
                return Ok(Some((entry_key.clone(), entry_value.clone())));
            }
        }
        Ok(None)
    }
}
