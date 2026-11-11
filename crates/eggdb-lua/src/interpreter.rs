use crate::LuaError;
use crate::ast::{BinaryOp, Block, Expr, Stat, TableField, UnaryOp};
use crate::host::Host;
use crate::value::{Closure, Function, TableRef, Value, number_text};
use std::cell::RefCell;
use std::rc::Rc;

const MOST_CALL_DEPTH: usize = 200;
const MOST_METATABLE_CHAIN: usize = 100;

/// A native function: it gets the interpreter, the host, its arguments and the calling line.
pub type Native = fn(&mut Interpreter, &mut dyn Host, Vec<Value>, u32) -> Result<Vec<Value>, Unwind>;

/// How a running script stops early: an error value (which `pcall` catches) or a kill (which it
/// does not), with the line.
#[derive(Debug, Clone)]
pub enum Unwind {
    /// An error raised with a value.
    Error(Value, u32),
    /// The script ran out of its budget.
    Kill(String),
}

pub(crate) fn raise<T>(line: u32, message: impl Into<String>) -> Result<T, Unwind> {
    Err(Unwind::Error(Value::string(message.into()), line))
}

/// One scope of local variables, with the function's extra arguments when it starts a function.
#[derive(Default)]
pub struct Frame {
    variables: RefCell<Vec<(String, Rc<RefCell<Value>>)>>,
    parent: Option<Rc<Frame>>,
    varargs: Option<Rc<Vec<Value>>>,
}

impl Frame {
    fn child(parent: &Rc<Frame>) -> Rc<Frame> {
        Rc::new(Frame {
            variables: RefCell::default(),
            parent: Some(Rc::clone(parent)),
            varargs: None,
        })
    }

    fn declare(&self, name: &str, value: Value) {
        self.variables
            .borrow_mut()
            .push((name.to_owned(), Rc::new(RefCell::new(value))));
    }

    fn lookup(&self, name: &str) -> Option<Rc<RefCell<Value>>> {
        if let Some((_, slot)) = self.variables.borrow().iter().rev().find(|(known, _)| known == name) {
            return Some(Rc::clone(slot));
        }
        self.parent.as_ref().and_then(|parent| parent.lookup(name))
    }

    fn varargs(&self) -> Rc<Vec<Value>> {
        match (&self.varargs, &self.parent) {
            (Some(varargs), _) => Rc::clone(varargs),
            (None, Some(parent)) => parent.varargs(),
            (None, None) => Rc::default(),
        }
    }
}

enum Flow {
    Normal,
    Break,
    Return(Vec<Value>),
}

/// A Lua state: its globals, and the step budget of the script running in it.
pub struct Interpreter {
    pub(crate) globals: TableRef,
    pub(crate) string_library: TableRef,
    pub(crate) json_null: TableRef,
    pub(crate) registered_functions: TableRef,
    steps_left: u64,
    depth: usize,
}

impl std::fmt::Debug for Interpreter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Interpreter").finish_non_exhaustive()
    }
}

fn adjusted(mut values: Vec<Value>, count: usize) -> Vec<Value> {
    values.resize(count, Value::Nil);
    values
}

fn first(values: Vec<Value>) -> Value {
    values.into_iter().next().unwrap_or(Value::Nil)
}

impl Interpreter {
    /// A state with the base library (and, through [`crate::stdlib`], the libraries Redis gives
    /// scripts) in its globals.
    pub fn new() -> Self {
        let mut interpreter = Self {
            globals: TableRef::new(),
            string_library: TableRef::new(),
            json_null: TableRef::new(),
            registered_functions: TableRef::new(),
            steps_left: 0,
            depth: 0,
        };
        crate::stdlib::install(&mut interpreter);
        interpreter
    }

    /// The table of global variables.
    pub fn globals(&self) -> &TableRef {
        &self.globals
    }

    /// Sets the global `name`.
    pub fn set_global(&mut self, name: &str, value: Value) {
        let _ = self.globals.set(Value::string(name), value);
    }

    pub(crate) fn register(&mut self, table: &TableRef, name: &'static str, native: Native) {
        let _ = table.set(Value::string(name), Value::Function(Function::Native(native, name)));
    }

    /// Runs `source` as a chunk against `host`, allowing at most `step_budget` steps, and
    /// returns what it returns.
    pub fn run(&mut self, source: &[u8], host: &mut dyn Host, step_budget: u64) -> Result<Vec<Value>, LuaError> {
        let block = crate::parser::parse(source)?;
        self.steps_left = step_budget;
        self.depth = 0;
        let frame = Rc::new(Frame {
            varargs: Some(Rc::default()),
            ..Frame::default()
        });
        match self.exec_block(&block, &frame, host) {
            Ok(Flow::Return(values)) => Ok(values),
            Ok(_) => Ok(Vec::new()),
            Err(unwind) => Err(self.error_of(unwind)),
        }
    }

    pub(crate) fn error_of(&self, unwind: Unwind) -> LuaError {
        match unwind {
            Unwind::Kill(message) => LuaError::runtime(0, message),
            Unwind::Error(value, line) => {
                let message = match &value {
                    Value::String(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    Value::Table(table) => match table.get(&Value::string("err")) {
                        Value::String(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                        _ => "error object is not a string".to_owned(),
                    },
                    other => number_text(other.to_number().unwrap_or(0.0)),
                };
                if message.starts_with("user_script:") {
                    LuaError::runtime(0, message)
                } else {
                    LuaError::runtime(line, message)
                }
            }
        }
    }

    fn step(&mut self) -> Result<(), Unwind> {
        if self.steps_left == 0 {
            return Err(Unwind::Kill(
                "ERR the script ran past its step budget and was stopped".to_owned(),
            ));
        }
        self.steps_left -= 1;
        Ok(())
    }

    fn exec_block(&mut self, block: &Block, frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Flow, Unwind> {
        let scope = Frame::child(frame);
        self.exec_statements(block, &scope, host)
    }

    fn exec_statements(&mut self, block: &Block, scope: &Rc<Frame>, host: &mut dyn Host) -> Result<Flow, Unwind> {
        for statement in block {
            match self.exec(statement, scope, host)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn loop_body(&mut self, body: &Block, frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Option<Flow>, Unwind> {
        Ok(match self.exec_statements(body, frame, host)? {
            Flow::Normal => None,
            Flow::Break => Some(Flow::Normal),
            returned => Some(returned),
        })
    }

    fn assign_name(&mut self, name: &str, value: Value, frame: &Rc<Frame>) {
        match frame.lookup(name) {
            Some(slot) => *slot.borrow_mut() = value,
            None => {
                let _ = self.globals.set(Value::string(name), value);
            }
        }
    }

    fn exec(&mut self, statement: &Stat, frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Flow, Unwind> {
        self.step()?;
        match statement {
            Stat::Local(names, values) => {
                let values = adjusted(self.eval_list(values, frame, host)?, names.len());
                for (name, value) in names.iter().zip(values) {
                    frame.declare(name, value);
                }
            }
            Stat::Assign(targets, values) => {
                let mut places = Vec::with_capacity(targets.len());
                for target in targets {
                    places.push(match target {
                        Expr::Index(object, key, ..) => {
                            Some((self.eval(object, frame, host)?, self.eval(key, frame, host)?))
                        }
                        _ => None,
                    });
                }
                let values = adjusted(self.eval_list(values, frame, host)?, targets.len());
                for ((target, place), value) in targets.iter().zip(places).zip(values) {
                    match (target, place) {
                        (Expr::Name(name), _) => self.assign_name(name, value, frame),
                        (_, Some((object, key))) => self.set_index(&object, key, value, host, 0)?,
                        _ => return raise(0, "cannot assign"),
                    }
                }
            }
            Stat::Call(call) => {
                self.eval_multi(call, frame, host)?;
            }
            Stat::Do(block) => return self.exec_block(block, frame, host),
            Stat::While(condition, body) => loop {
                if !self.eval(condition, frame, host)?.truthy() {
                    break;
                }
                let scope = Frame::child(frame);
                if let Some(flow) = self.loop_body(body, &scope, host)? {
                    return Ok(flow);
                }
            },
            Stat::Repeat(body, condition) => loop {
                let scope = Frame::child(frame);
                if let Some(flow) = self.loop_body(body, &scope, host)? {
                    return Ok(flow);
                }
                if self.eval(condition, &scope, host)?.truthy() {
                    break;
                }
            },
            Stat::If(branches, otherwise) => {
                for (condition, body) in branches {
                    if self.eval(condition, frame, host)?.truthy() {
                        return self.exec_block(body, frame, host);
                    }
                }
                if let Some(body) = otherwise {
                    return self.exec_block(body, frame, host);
                }
            }
            Stat::NumericFor {
                variable,
                start,
                limit,
                step,
                body,
            } => {
                let number = |interpreter: &mut Self, expr: &Expr, what: &str, host: &mut dyn Host| -> Result<f64, Unwind> {
                    match interpreter.eval(expr, frame, host)?.to_number() {
                        Some(number) => Ok(number),
                        None => raise(0, format!("'for' {what} must be a number")),
                    }
                };
                let start = number(self, start, "initial value", host)?;
                let limit = number(self, limit, "limit", host)?;
                let step = match step {
                    Some(step) => number(self, step, "step", host)?,
                    None => 1.0,
                };
                let mut current = start;
                while (step > 0.0 && current <= limit) || (step <= 0.0 && current >= limit) {
                    self.step()?;
                    let scope = Frame::child(frame);
                    scope.declare(variable, Value::Number(current));
                    if let Some(flow) = self.loop_body(body, &scope, host)? {
                        return Ok(flow);
                    }
                    current += step;
                }
            }
            Stat::GenericFor {
                variables,
                values,
                body,
            } => {
                let mut state = adjusted(self.eval_list(values, frame, host)?, 3).into_iter();
                let (function, invariant, mut control) = (
                    state.next().unwrap_or(Value::Nil),
                    state.next().unwrap_or(Value::Nil),
                    state.next().unwrap_or(Value::Nil),
                );
                loop {
                    let results = adjusted(
                        self.call(&function, vec![invariant.clone(), control.clone()], host, 0)?,
                        variables.len(),
                    );
                    if matches!(results.first(), Some(Value::Nil) | None) {
                        break;
                    }
                    control = results[0].clone();
                    let scope = Frame::child(frame);
                    for (name, value) in variables.iter().zip(results) {
                        scope.declare(name, value);
                    }
                    if let Some(flow) = self.loop_body(body, &scope, host)? {
                        return Ok(flow);
                    }
                }
            }
            Stat::Function(path, method, body) => {
                let closure = Value::Function(Function::Lua(Rc::new(Closure {
                    body: Rc::clone(body),
                    frame: Rc::clone(frame),
                })));
                match (path.as_slice(), method) {
                    ([name], None) => self.assign_name(name, closure, frame),
                    ([first, rest @ ..], _) => {
                        let mut target = self.eval(&Expr::Name(first.clone()), frame, host)?;
                        let (fields, last): (&[String], &String) = match method {
                            Some(method) => (rest, method),
                            None => match rest.split_last() {
                                Some((last, fields)) => (fields, last),
                                None => return raise(0, "function name expected"),
                            },
                        };
                        for field in fields {
                            target = self.index(&target, &Value::string(field), host, 0)?;
                        }
                        self.set_index(&target, Value::string(last), closure, host, 0)?;
                    }
                    ([], _) => return raise(0, "function name expected"),
                }
            }
            Stat::LocalFunction(name, body) => {
                frame.declare(name, Value::Nil);
                let closure = Value::Function(Function::Lua(Rc::new(Closure {
                    body: Rc::clone(body),
                    frame: Rc::clone(frame),
                })));
                self.assign_name(name, closure, frame);
            }
            Stat::Return(values) => return Ok(Flow::Return(self.eval_list(values, frame, host)?)),
            Stat::Break => return Ok(Flow::Break),
        }
        Ok(Flow::Normal)
    }

    pub(crate) fn eval_list(&mut self, exprs: &[Expr], frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Vec<Value>, Unwind> {
        let mut values = Vec::with_capacity(exprs.len());
        for (at, expr) in exprs.iter().enumerate() {
            if at + 1 == exprs.len() {
                values.extend(self.eval_multi(expr, frame, host)?);
            } else {
                values.push(self.eval(expr, frame, host)?);
            }
        }
        Ok(values)
    }

    fn eval_multi(&mut self, expr: &Expr, frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Vec<Value>, Unwind> {
        match expr {
            Expr::Call(function, arguments, line) => {
                let function = self.eval(function, frame, host)?;
                let arguments = self.eval_list(arguments, frame, host)?;
                self.call(&function, arguments, host, *line)
            }
            Expr::Method(object, name, arguments, line) => {
                let object = self.eval(object, frame, host)?;
                let function = match &object {
                    Value::String(_) => self.string_library.get(&Value::string(name)),
                    _ => self.index(&object, &Value::string(name), host, *line)?,
                };
                let mut all = vec![object];
                all.extend(self.eval_list(arguments, frame, host)?);
                self.call(&function, all, host, *line)
            }
            Expr::VarArgs => Ok(frame.varargs().as_ref().clone()),
            other => Ok(vec![self.eval(other, frame, host)?]),
        }
    }

    fn eval(&mut self, expr: &Expr, frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Value, Unwind> {
        Ok(match expr {
            Expr::Nil => Value::Nil,
            Expr::True => Value::Boolean(true),
            Expr::False => Value::Boolean(false),
            Expr::Number(number) => Value::Number(*number),
            Expr::String(bytes) => Value::String(Rc::clone(bytes)),
            Expr::VarArgs | Expr::Call(..) | Expr::Method(..) => first(self.eval_multi(expr, frame, host)?),
            Expr::Function(body) => Value::Function(Function::Lua(Rc::new(Closure {
                body: Rc::clone(body),
                frame: Rc::clone(frame),
            }))),
            Expr::Table(fields) => self.table(fields, frame, host)?,
            Expr::Binary(op, left, right) => self.binary(*op, left, right, frame, host)?,
            Expr::Unary(op, operand) => {
                let value = self.eval(operand, frame, host)?;
                match op {
                    UnaryOp::Not => Value::Boolean(!value.truthy()),
                    UnaryOp::Negate => match value.to_number() {
                        Some(number) => Value::Number(-number),
                        None => return raise(0, format!("attempt to perform arithmetic on a {} value", value.type_name())),
                    },
                    UnaryOp::Length => match &value {
                        Value::String(bytes) => Value::Number(bytes.len() as f64),
                        Value::Table(table) => Value::Number(table.length() as f64),
                        other => return raise(0, format!("attempt to get length of a {} value", other.type_name())),
                    },
                }
            }
            Expr::Name(name) => match frame.lookup(name) {
                Some(slot) => slot.borrow().clone(),
                None => self.globals.get(&Value::string(name)),
            },
            Expr::Index(object, key) => {
                let object = self.eval(object, frame, host)?;
                let key = self.eval(key, frame, host)?;
                self.index(&object, &key, host, 0)?
            }
            Expr::Paren(inner) => self.eval(inner, frame, host)?,
        })
    }

    fn table(&mut self, fields: &[TableField], frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Value, Unwind> {
        let table = TableRef::new();
        let mut position = 1.0;
        for (at, field) in fields.iter().enumerate() {
            self.step()?;
            match field {
                TableField::Positional(expr) if at + 1 == fields.len() => {
                    for value in self.eval_multi(expr, frame, host)? {
                        table.set(Value::Number(position), value).map_err(|message| Unwind::Error(Value::string(message), 0))?;
                        position += 1.0;
                    }
                }
                TableField::Positional(expr) => {
                    let value = self.eval(expr, frame, host)?;
                    table.set(Value::Number(position), value).map_err(|message| Unwind::Error(Value::string(message), 0))?;
                    position += 1.0;
                }
                TableField::Named(name, expr) => {
                    let value = self.eval(expr, frame, host)?;
                    table.set(Value::string(name), value).map_err(|message| Unwind::Error(Value::string(message), 0))?;
                }
                TableField::Keyed(key, expr) => {
                    let key = self.eval(key, frame, host)?;
                    let value = self.eval(expr, frame, host)?;
                    table.set(key, value).map_err(|message| Unwind::Error(Value::string(message), 0))?;
                }
            }
        }
        Ok(Value::Table(table))
    }

    fn binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr, frame: &Rc<Frame>, host: &mut dyn Host) -> Result<Value, Unwind> {
        if op == BinaryOp::And {
            let left = self.eval(left, frame, host)?;
            return if left.truthy() { self.eval(right, frame, host) } else { Ok(left) };
        }
        if op == BinaryOp::Or {
            let left = self.eval(left, frame, host)?;
            return if left.truthy() { Ok(left) } else { self.eval(right, frame, host) };
        }
        let left = self.eval(left, frame, host)?;
        let right = self.eval(right, frame, host)?;
        let arithmetic = |left: &Value, right: &Value| -> Result<(f64, f64), Unwind> {
            match (left.to_number(), right.to_number()) {
                (Some(left), Some(right)) => Ok((left, right)),
                (None, _) => raise(0, format!("attempt to perform arithmetic on a {} value", left.type_name())),
                (_, None) => raise(0, format!("attempt to perform arithmetic on a {} value", right.type_name())),
            }
        };
        Ok(match op {
            BinaryOp::Add => arithmetic(&left, &right).map(|(a, b)| Value::Number(a + b))?,
            BinaryOp::Subtract => arithmetic(&left, &right).map(|(a, b)| Value::Number(a - b))?,
            BinaryOp::Multiply => arithmetic(&left, &right).map(|(a, b)| Value::Number(a * b))?,
            BinaryOp::Divide => arithmetic(&left, &right).map(|(a, b)| Value::Number(a / b))?,
            BinaryOp::Modulo => arithmetic(&left, &right).map(|(a, b)| Value::Number(a - (a / b).floor() * b))?,
            BinaryOp::Power => arithmetic(&left, &right).map(|(a, b)| Value::Number(a.powf(b)))?,
            BinaryOp::Concat => match (left.to_bytes(), right.to_bytes()) {
                (Some(mut joined), Some(tail)) => {
                    self.step()?;
                    joined.extend_from_slice(&tail);
                    Value::String(joined.into())
                }
                (None, _) => return raise(0, format!("attempt to concatenate a {} value", left.type_name())),
                (_, None) => return raise(0, format!("attempt to concatenate a {} value", right.type_name())),
            },
            BinaryOp::Equal => Value::Boolean(left == right),
            BinaryOp::NotEqual => Value::Boolean(left != right),
            BinaryOp::Less | BinaryOp::Greater | BinaryOp::LessEqual | BinaryOp::GreaterEqual => {
                let ordering = match (&left, &right) {
                    (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
                    (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
                    _ => {
                        return raise(0, format!("attempt to compare {} with {}", left.type_name(), right.type_name()));
                    }
                };
                Value::Boolean(match (op, ordering) {
                    (_, None) => false,
                    (BinaryOp::Less, Some(order)) => order.is_lt(),
                    (BinaryOp::Greater, Some(order)) => order.is_gt(),
                    (BinaryOp::LessEqual, Some(order)) => order.is_le(),
                    (_, Some(order)) => order.is_ge(),
                })
            }
            BinaryOp::And | BinaryOp::Or => Value::Nil,
        })
    }

    fn metamethod(&self, value: &Value, name: &str) -> Value {
        match value {
            Value::Table(table) => match &table.0.borrow().metatable {
                Some(metatable) => metatable.get(&Value::string(name)),
                None => Value::Nil,
            },
            _ => Value::Nil,
        }
    }

    /// `object[key]`, following `__index` metamethods.
    pub(crate) fn index(&mut self, object: &Value, key: &Value, host: &mut dyn Host, line: u32) -> Result<Value, Unwind> {
        let mut current = object.clone();
        for _ in 0..MOST_METATABLE_CHAIN {
            match &current {
                Value::Table(table) => {
                    let raw = table.get(key);
                    if !matches!(raw, Value::Nil) {
                        return Ok(raw);
                    }
                    match self.metamethod(&current, "__index") {
                        Value::Nil => return Ok(Value::Nil),
                        handler @ Value::Function(_) => {
                            return Ok(first(self.call(&handler, vec![current.clone(), key.clone()], host, line)?));
                        }
                        next => current = next,
                    }
                }
                Value::String(_) => return Ok(self.string_library.get(key)),
                other => return raise(line, format!("attempt to index a {} value", other.type_name())),
            }
        }
        raise(line, "'__index' chain too long; possible loop")
    }

    /// `object[key] = value`, following `__newindex` metamethods.
    pub(crate) fn set_index(&mut self, object: &Value, key: Value, value: Value, host: &mut dyn Host, line: u32) -> Result<(), Unwind> {
        self.step()?;
        let Value::Table(table) = object else {
            return raise(line, format!("attempt to index a {} value", object.type_name()));
        };
        if matches!(table.get(&key), Value::Nil) {
            match self.metamethod(object, "__newindex") {
                Value::Nil => {}
                handler @ Value::Function(_) => {
                    self.call(&handler, vec![object.clone(), key, value], host, line)?;
                    return Ok(());
                }
                next @ Value::Table(_) => return self.set_index(&next, key, value, host, line),
                _ => {}
            }
        }
        table.set(key, value).map_err(|message| Unwind::Error(Value::string(message), line))
    }

    /// Calls `function` with `arguments` and returns all its results.
    pub(crate) fn call(&mut self, function: &Value, arguments: Vec<Value>, host: &mut dyn Host, line: u32) -> Result<Vec<Value>, Unwind> {
        self.step()?;
        if self.depth >= MOST_CALL_DEPTH {
            return raise(line, "stack overflow");
        }
        self.depth += 1;
        let outcome = self.call_inner(function, arguments, host, line);
        self.depth -= 1;
        outcome
    }

    fn call_inner(&mut self, function: &Value, mut arguments: Vec<Value>, host: &mut dyn Host, line: u32) -> Result<Vec<Value>, Unwind> {
        match function {
            Value::Function(Function::Native(native, _)) => native(self, host, arguments, line),
            Value::Function(Function::Lua(closure)) => {
                let body = &closure.body;
                let extra = if arguments.len() > body.parameters.len() {
                    arguments.split_off(body.parameters.len())
                } else {
                    Vec::new()
                };
                let frame = Rc::new(Frame {
                    variables: RefCell::default(),
                    parent: Some(Rc::clone(&closure.frame)),
                    varargs: Some(Rc::new(if body.variadic { extra } else { Vec::new() })),
                });
                let mut supplied = arguments.into_iter();
                for parameter in &body.parameters {
                    frame.declare(parameter, supplied.next().unwrap_or(Value::Nil));
                }
                match self.exec_statements(&body.body, &frame, host)? {
                    Flow::Return(values) => Ok(values),
                    _ => Ok(Vec::new()),
                }
            }
            other => match self.metamethod(other, "__call") {
                Value::Nil => raise(line, format!("attempt to call a {} value", other.type_name())),
                handler => {
                    let mut all = vec![other.clone()];
                    all.extend(arguments);
                    self.call(&handler, all, host, line)
                }
            },
        }
    }

    /// The text `tostring` gives `value`, using `__tostring`.
    pub(crate) fn text_of(&mut self, value: &Value, host: &mut dyn Host) -> Result<Vec<u8>, Unwind> {
        Ok(match value {
            Value::Nil => b"nil".to_vec(),
            Value::Boolean(flag) => flag.to_string().into_bytes(),
            Value::Number(number) => number_text(*number).into_bytes(),
            Value::String(bytes) => bytes.to_vec(),
            Value::Table(table) => match self.metamethod(value, "__tostring") {
                Value::Nil => format!("table: 0x{:08x}", table.identity()).into_bytes(),
                handler => {
                    let text = first(self.call(&handler, vec![value.clone()], host, 0)?);
                    text.to_bytes().unwrap_or_default()
                }
            },
            Value::Function(_) => b"function: builtin".to_vec(),
        })
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
