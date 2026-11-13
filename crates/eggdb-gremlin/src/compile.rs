use crate::traversal::{Arg, Predicate, Step};
use eggdb_graph::Value;

/// How the rows of a compiled traversal turn into Gremlin results.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Shape {
    Plain,
    ValueMap { tokens: bool, keys: Vec<String> },
    ElementMap { keys: Vec<String> },
    GroupCount,
    Path,
    Nothing,
}

/// A traversal as one Cypher query over EggDB's property graph.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compiled {
    pub(crate) query: String,
    pub(crate) parameters: Vec<(String, Value)>,
    pub(crate) shape: Shape,
    pub(crate) writes: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Current {
    Start,
    Vertex(String),
    Edge { name: String, out_vertex: Option<String>, in_vertex: Option<String>, other: String },
    Value(String),
}

impl Current {
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Self::Start => None,
            Self::Vertex(name) | Self::Value(name) => Some(name),
            Self::Edge { name, .. } => Some(name),
        }
    }
}

pub(crate) struct PendingEdge {
    label: String,
    from: Option<String>,
    to: Option<String>,
    properties: Vec<(String, String)>,
}

pub(crate) enum Pending {
    None,
    Edge(PendingEdge),
    Order(Vec<(String, bool)>),
    GroupCount(Option<String>),
    Select(Vec<String>, Vec<String>),
    Repeat(crate::branch::Repeat),
}

pub(crate) struct Compiler {
    clauses: Vec<String>,
    parameters: Vec<(String, Value)>,
    pub(crate) scope: Vec<String>,
    pub(crate) path: Vec<String>,
    pub(crate) labels: Vec<(String, Current)>,
    pub(crate) current: Current,
    counter: usize,
    after_write: bool,
    open_match: Option<bool>,
    pub(crate) pending: Pending,
    shape: Shape,
    writes: bool,
    finished: bool,
}

pub(crate) type Compiling<T> = Result<T, String>;

pub(crate) fn unsupported(step: &str) -> String {
    format!("the Gremlin step {step}() is not supported by EggDB yet")
}

pub(crate) fn text_arg(arg: Option<&Arg>, what: &str) -> Compiling<String> {
    match arg {
        Some(Arg::Value(Value::String(text))) => Ok(text.clone()),
        _ => Err(format!("{what} is a string")),
    }
}

pub(crate) fn texts(args: &[Arg], what: &str) -> Compiling<Vec<String>> {
    args.iter().map(|arg| text_arg(Some(arg), what)).collect()
}

pub(crate) fn count_arg(arg: Option<&Arg>, what: &str) -> Compiling<i64> {
    match arg {
        Some(Arg::Value(Value::Integer(number))) if *number >= -1 => Ok(*number),
        _ => Err(format!("{what} is a whole number")),
    }
}

pub(crate) fn identifier(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_') && !name.starts_with(|character: char| character.is_ascii_digit()) {
        name.to_owned()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

pub(crate) fn type_list(labels: &[String]) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!(":{}", labels.iter().map(|label| identifier(label)).collect::<Vec<_>>().join("|"))
    }
}

pub(crate) fn id_values(args: &[Arg]) -> Compiling<Vec<Value>> {
    let mut ids = Vec::new();
    for arg in args {
        match arg {
            Arg::Value(Value::List(items)) => ids.extend(items.iter().cloned()),
            Arg::Value(Value::Node(node)) => ids.push(Value::Integer(i64::try_from(node.id).unwrap_or(i64::MAX))),
            Arg::Value(Value::Relationship(relationship)) => ids.push(Value::Integer(i64::try_from(relationship.id).unwrap_or(i64::MAX))),
            Arg::Value(value) => ids.push(value.clone()),
            _ => return Err("element ids are values".into()),
        }
    }
    Ok(ids)
}

impl Compiler {
    pub(crate) fn fresh(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}{}", self.counter)
    }

    pub(crate) fn parameter(&mut self, value: Value) -> String {
        let name = format!("p{}", self.parameters.len());
        self.parameters.push((name.clone(), value));
        format!("${name}")
    }

    fn scope_list(&self) -> String {
        self.scope.join(", ")
    }

    fn clause(&mut self, text: String) {
        self.clauses.push(text);
        self.open_match = None;
    }

    pub(crate) fn matching(&mut self, pattern: String) {
        self.after_writes();
        self.clauses.push(format!("MATCH {pattern}"));
        self.open_match = Some(false);
    }

    pub(crate) fn filter(&mut self, condition: String) {
        match self.open_match {
            Some(has_where) => {
                if let Some(last) = self.clauses.last_mut() {
                    last.push_str(if has_where { " AND " } else { " WHERE " });
                    last.push_str(&condition);
                }
                self.open_match = Some(true);
            }
            None => {
                let scope = self.scope_list();
                self.clause(format!("WITH {scope} WHERE {condition}"));
                self.after_write = false;
                self.open_match = Some(true);
            }
        }
    }

    fn writing(&mut self, text: String) {
        self.clause(text);
        self.writes = true;
        self.after_write = true;
    }

    pub(crate) fn enter(&mut self, current: Current) {
        if let Some(name) = current.name() {
            if !self.scope.iter().any(|known| known == name) {
                self.scope.push(name.to_owned());
            }
            self.path.push(name.to_owned());
        }
        self.current = current;
    }

    fn element(&self, step: &str) -> Compiling<String> {
        match &self.current {
            Current::Vertex(name) | Current::Edge { name, .. } => Ok(name.clone()),
            _ => Err(format!("{step}() follows a step that reaches vertices or edges")),
        }
    }

    pub(crate) fn vertex(&self, step: &str) -> Compiling<String> {
        match &self.current {
            Current::Vertex(name) => Ok(name.clone()),
            _ => Err(format!("{step}() follows a step that reaches vertices")),
        }
    }

    pub(crate) fn expression(&self, step: &str) -> Compiling<String> {
        self.current.name().map(str::to_owned).ok_or_else(|| format!("{step}() needs a traversal before it"))
    }

    fn property_expression(&self, key: &str, step: &str) -> Compiling<String> {
        Ok(format!("{}.{}", self.element(step)?, identifier(key)))
    }

    fn reduce_to(&mut self, expression: String) {
        let name = self.fresh("gv");
        self.clause(format!("WITH {expression} AS {name}"));
        self.after_write = false;
        self.scope = vec![name.clone()];
        self.path.clear();
        self.labels.clear();
        self.enter(Current::Value(name));
    }

    fn after_writes(&mut self) {
        if self.after_write && !self.scope.is_empty() {
            let scope = self.scope_list();
            self.clause(format!("WITH {scope}"));
        }
        self.after_write = false;
    }

    pub(crate) fn unwind(&mut self, list: String) {
        self.after_writes();
        let name = self.fresh("gv");
        self.clause(format!("UNWIND {list} AS {name}"));
        self.after_write = false;
        self.enter(Current::Value(name));
    }

    fn predicate(&mut self, subject: &str, predicate: &Predicate) -> Compiling<String> {
        let first = predicate.values.first().cloned().unwrap_or(Value::Null);
        let second = predicate.values.get(1).cloned().unwrap_or(Value::Null);
        let operator = |compiler: &mut Self, operator: &str, value: Value| format!("{subject} {operator} {}", compiler.parameter(value));
        Ok(match predicate.name.as_str() {
            "eq" => operator(self, "=", first),
            "neq" => operator(self, "<>", first),
            "gt" => operator(self, ">", first),
            "gte" => operator(self, ">=", first),
            "lt" => operator(self, "<", first),
            "lte" => operator(self, "<=", first),
            "within" => operator(self, "IN", Value::List(predicate.values.clone())),
            "without" => format!("NOT ({})", operator(self, "IN", Value::List(predicate.values.clone()))),
            "between" => format!("({} AND {})", operator(self, ">=", first), operator(self, "<", second)),
            "inside" => format!("({} AND {})", operator(self, ">", first), operator(self, "<", second)),
            "outside" => format!("({} OR {})", operator(self, "<", first), operator(self, ">", second)),
            "startingWith" => operator(self, "STARTS WITH", first),
            "endingWith" => operator(self, "ENDS WITH", first),
            "containing" => operator(self, "CONTAINS", first),
            "notStartingWith" => format!("NOT ({})", operator(self, "STARTS WITH", first)),
            "notEndingWith" => format!("NOT ({})", operator(self, "ENDS WITH", first)),
            "notContaining" => format!("NOT ({})", operator(self, "CONTAINS", first)),
            other => return Err(format!("{other} is not a predicate EggDB knows")),
        })
    }

    pub(crate) fn test(&mut self, subject: &str, arg: &Arg) -> Compiling<String> {
        match arg {
            Arg::Predicate(predicate) => self.predicate(subject, predicate),
            Arg::Value(value) => {
                let parameter = self.parameter(value.clone());
                Ok(format!("{subject} = {parameter}"))
            }
            _ => Err("has() and is() compare with a value or a predicate".into()),
        }
    }

    fn label_condition(&mut self, labels: &[String]) -> Compiling<String> {
        match &self.current {
            Current::Vertex(name) => {
                let name = name.clone();
                Ok(format!("({})", labels.iter().map(|label| format!("{name}:{}", identifier(label))).collect::<Vec<_>>().join(" OR ")))
            }
            Current::Edge { name, .. } => {
                let name = name.clone();
                let parameter = self.parameter(Value::List(labels.iter().cloned().map(Value::String).collect()));
                Ok(format!("type({name}) IN {parameter}"))
            }
            _ => Err("hasLabel() follows a step that reaches vertices or edges".into()),
        }
    }

    fn id_condition(&mut self, name: &str, ids: Vec<Value>) -> String {
        let parameter = self.parameter(Value::List(ids));
        format!("id({name}) IN {parameter}")
    }

    fn label_expression(&self, step: &str) -> Compiling<String> {
        match &self.current {
            Current::Vertex(name) => Ok(format!("head(labels({name}))")),
            Current::Edge { name, .. } => Ok(format!("type({name})")),
            _ => Err(format!("{step} reads the label of vertices or edges")),
        }
    }

    fn by_expression(&self, args: &[Arg]) -> Compiling<(String, bool)> {
        let descending = args.iter().any(|arg| matches!(arg, Arg::Token(token) if matches!(token.as_str(), "desc" | "decr")));
        let expression = match args.first() {
            None => self.expression("by")?,
            Some(Arg::Token(token)) if matches!(token.as_str(), "desc" | "decr" | "asc" | "incr") => self.expression("by")?,
            Some(Arg::Token(token)) if token == "label" => self.label_expression("by(label)")?,
            Some(Arg::Token(token)) if token == "id" => format!("id({})", self.element("by(id)")?),
            Some(Arg::Value(Value::String(key))) => self.property_expression(key, "by")?,
            Some(Arg::Traversal(steps)) => match steps.as_slice() {
                [Step { name, args }] if name == "values" && args.len() == 1 => {
                    self.property_expression(&text_arg(args.first(), "values()")?, "by")?
                }
                [Step { name, .. }] if name == "label" => self.label_expression("by(label())")?,
                [Step { name, .. }] if name == "id" => format!("id({})", self.element("by(id())")?),
                _ => return Err("by() takes a property key, label, id or values('key')".into()),
            },
            Some(_) => return Err("by() takes a property key, label, id or values('key')".into()),
        };
        Ok((expression, descending))
    }

    fn endpoint(&mut self, arg: Option<&Arg>, step: &str) -> Compiling<String> {
        match arg {
            Some(Arg::Value(Value::String(label))) => match self.labels.iter().find(|(name, _)| name == label) {
                Some((_, Current::Vertex(name))) => Ok(name.clone()),
                _ => Err(format!("{step}('{label}') names a step label of a vertex")),
            },
            Some(Arg::Traversal(steps)) => match steps.as_slice() {
                [Step { name, args }] if name == "V" => {
                    let ids = id_values(args)?;
                    let vertex = self.fresh("v");
                    let condition = self.id_condition(&vertex, ids);
                    self.matching(format!("({vertex})"));
                    self.filter(condition);
                    if !self.scope.contains(&vertex) {
                        self.scope.push(vertex.clone());
                    }
                    Ok(vertex)
                }
                [Step { name, args }] if name == "select" => self.endpoint(args.first(), step),
                _ => Err(format!("{step}() takes a step label, __.V(id) or a vertex")),
            },
            Some(Arg::Value(Value::Node(_) | Value::Integer(_))) => {
                let ids = id_values(std::slice::from_ref(arg.unwrap_or(&Arg::Value(Value::Null))))?;
                let vertex = self.fresh("v");
                let condition = self.id_condition(&vertex, ids);
                self.matching(format!("({vertex})"));
                self.filter(condition);
                if !self.scope.contains(&vertex) {
                    self.scope.push(vertex.clone());
                }
                Ok(vertex)
            }
            _ => Err(format!("{step}() takes a step label, __.V(id) or a vertex")),
        }
    }

    fn flush(&mut self) -> Compiling<()> {
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::None => {}
            Pending::Edge(edge) => {
                let from = edge.from.ok_or("addE() needs from() or a vertex before it")?;
                let to = edge.to.ok_or("addE() needs to()")?;
                let name = self.fresh("e");
                let properties = if edge.properties.is_empty() {
                    String::new()
                } else {
                    format!(
                        " {{{}}}",
                        edge.properties.iter().map(|(key, parameter)| format!("{}: {parameter}", identifier(key))).collect::<Vec<_>>().join(", ")
                    )
                };
                self.writing(format!("CREATE ({from})-[{name}:{}{properties}]->({to})", identifier(&edge.label)));
                self.enter(Current::Edge { name, out_vertex: Some(from.clone()), in_vertex: Some(to.clone()), other: to });
            }
            Pending::Order(keys) => {
                let keys = if keys.is_empty() { vec![(self.expression("order")?, false)] } else { keys };
                let scope = self.scope_list();
                let order = keys
                    .iter()
                    .map(|(expression, descending)| format!("{expression}{}", if *descending { " DESC" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.clause(format!("WITH {scope} ORDER BY {order}"));
                self.after_write = false;
            }
            Pending::GroupCount(key) => {
                let key = match key {
                    Some(key) => key,
                    None => self.expression("groupCount")?,
                };
                self.clause(format!("WITH {key} AS gk, count(*) AS gc"));
                self.after_write = false;
                self.scope = vec!["gk".into(), "gc".into()];
                self.shape = Shape::GroupCount;
                self.finished = true;
            }
            Pending::Repeat(repeat) => self.repeated(repeat)?,
            Pending::Select(labels, keys) => {
                let mut fields = Vec::new();
                for (index, label) in labels.iter().enumerate() {
                    let (_, found) = self
                        .labels
                        .iter()
                        .find(|(name, _)| name == label)
                        .ok_or_else(|| format!("select('{label}') names no step label"))?;
                    let name = found.name().unwrap_or_default().to_owned();
                    let field = match keys.get(index % keys.len().max(1)) {
                        Some(key) if !keys.is_empty() => format!("{name}.{}", identifier(key)),
                        _ => name,
                    };
                    fields.push((label.clone(), field));
                }
                if let [(_, field)] = fields.as_slice() {
                    let field = field.clone();
                    match self.labels.iter().find(|(name, _)| *name == labels[0]).map(|(_, found)| found.clone()) {
                        Some(found) if keys.is_empty() => {
                            self.path.push(field);
                            self.current = found;
                        }
                        _ => self.unwind(format!("[{field}]")),
                    }
                } else {
                    let map = fields.iter().map(|(label, field)| format!("{}: {field}", identifier(label))).collect::<Vec<_>>().join(", ");
                    self.unwind(format!("[{{{map}}}]"));
                }
            }
        }
        Ok(())
    }

    fn modulate(&mut self, step: &Step) -> Compiling<bool> {
        let pending = match self.pending {
            Pending::None => "",
            Pending::Edge(_) => "edge",
            Pending::Order(_) => "order",
            Pending::GroupCount(_) => "groupCount",
            Pending::Select(..) => "select",
            Pending::Repeat(_) => "repeat",
        };
        match (pending, step.name.as_str()) {
            ("edge", "from" | "to") => {
                let endpoint = self.endpoint(step.args.first(), &step.name)?;
                if let Pending::Edge(edge) = &mut self.pending {
                    if step.name == "from" {
                        edge.from = Some(endpoint);
                    } else {
                        edge.to = Some(endpoint);
                    }
                }
                Ok(true)
            }
            ("edge", "property") => {
                let (key, value) = property_args(&step.args)?;
                let parameter = self.parameter(value);
                if let Pending::Edge(edge) = &mut self.pending {
                    edge.properties.push((key, parameter));
                }
                Ok(true)
            }
            ("order", "by") => {
                let key = self.by_expression(&step.args)?;
                if let Pending::Order(keys) = &mut self.pending {
                    keys.push(key);
                }
                Ok(true)
            }
            ("groupCount", "by") => {
                let (key, _) = self.by_expression(&step.args)?;
                if let Pending::GroupCount(found) = &mut self.pending {
                    *found = Some(key);
                }
                Ok(true)
            }
            ("repeat", "times" | "emit" | "until") => {
                self.modulate_repeat(step)?;
                Ok(true)
            }
            ("select", "by") => {
                let key = match step.args.first() {
                    Some(Arg::Value(Value::String(key))) => key.clone(),
                    _ => return Err("select(...).by() takes a property key".into()),
                };
                if let Pending::Select(_, keys) = &mut self.pending {
                    keys.push(key);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn navigate(&mut self, step: &Step) -> Compiling<()> {
        let from = self.vertex(&step.name)?;
        let labels = texts(&step.args, "an edge label")?;
        let types = type_list(&labels);
        match step.name.as_str() {
            "out" | "in" | "both" => {
                let to = self.fresh("v");
                let pattern = match step.name.as_str() {
                    "out" => format!("({from})-[{types}]->({to})"),
                    "in" => format!("({from})<-[{types}]-({to})"),
                    _ => format!("({from})-[{types}]-({to})"),
                };
                self.matching(pattern);
                self.enter(Current::Vertex(to));
            }
            _ => {
                let edge = self.fresh("e");
                let other = self.fresh("v");
                let (pattern, out_vertex, in_vertex) = match step.name.as_str() {
                    "outE" => (format!("({from})-[{edge}{types}]->({other})"), Some(from.clone()), Some(other.clone())),
                    "inE" => (format!("({from})<-[{edge}{types}]-({other})"), Some(other.clone()), Some(from.clone())),
                    _ => (format!("({from})-[{edge}{types}]-({other})"), None, None),
                };
                self.matching(pattern);
                if !self.scope.contains(&other) {
                    self.scope.push(other.clone());
                }
                self.enter(Current::Edge { name: edge, out_vertex, in_vertex, other });
            }
        }
        Ok(())
    }

    fn edge_end(&mut self, step: &str) -> Compiling<()> {
        let Current::Edge { name, out_vertex, in_vertex, other } = self.current.clone() else {
            return Err(format!("{step}() follows a step that reaches edges"));
        };
        match step {
            "otherV" if !other.is_empty() => self.enter(Current::Vertex(other)),
            "otherV" => return Err("otherV() needs to know where the edge was reached from; use inV() or outV()".into()),
            "inV" | "outV" => {
                let known = if step == "inV" { in_vertex } else { out_vertex };
                match known {
                    Some(vertex) => self.enter(Current::Vertex(vertex)),
                    None => {
                        let function = if step == "inV" { "endNode" } else { "startNode" };
                        let vertex = self.fresh("v");
                        self.clause(format!("WITH {}, {function}({name}) AS {vertex}", self.scope_list()));
                        self.enter(Current::Vertex(vertex));
                    }
                }
            }
            _ => {
                self.after_writes();
                let vertex = self.fresh("v");
                self.clause(format!("UNWIND [startNode({name}), endNode({name})] AS {vertex}"));
                self.enter(Current::Vertex(vertex));
            }
        }
        Ok(())
    }

    fn has(&mut self, args: &[Arg]) -> Compiling<()> {
        let element = self.element("has")?;
        match args {
            [Arg::Value(Value::String(key))] => {
                let condition = format!("{element}.{} IS NOT NULL", identifier(key));
                self.filter(condition);
            }
            [Arg::Token(token), test] if token == "label" => {
                let condition = match test {
                    Arg::Value(Value::String(label)) => self.label_condition(std::slice::from_ref(label))?,
                    other => {
                        let subject = self.label_expression("has(T.label)")?;
                        self.test(&subject, other)?
                    }
                };
                self.filter(condition);
            }
            [Arg::Token(token), test] if token == "id" => {
                let condition = self.test(&format!("id({element})"), test)?;
                self.filter(condition);
            }
            [Arg::Value(Value::String(key)), test] => {
                let condition = self.test(&format!("{element}.{}", identifier(key)), test)?;
                self.filter(condition);
            }
            [Arg::Value(Value::String(label)), Arg::Value(Value::String(key)), test] => {
                let labelled = self.label_condition(std::slice::from_ref(label))?;
                let condition = self.test(&format!("{element}.{}", identifier(key)), test)?;
                self.filter(format!("{labelled} AND {condition}"));
            }
            _ => return Err("has() takes a key, a key and a value or predicate, or a label, key and value".into()),
        }
        Ok(())
    }

    fn step(&mut self, step: &Step) -> Compiling<()> {
        if self.modulate(step)? {
            return Ok(());
        }
        self.flush()?;
        if self.finished {
            return Err(format!("{}() cannot follow groupCount() or drop() in EggDB yet", step.name));
        }
        let args = &step.args;
        match step.name.as_str() {
            "V" => {
                let vertex = self.fresh("v");
                self.matching(format!("({vertex})"));
                if !args.is_empty() {
                    let ids = id_values(args)?;
                    let condition = self.id_condition(&vertex, ids);
                    self.filter(condition);
                }
                self.enter(Current::Vertex(vertex));
            }
            "E" => {
                let (start, edge, end) = (self.fresh("v"), self.fresh("e"), self.fresh("v"));
                self.matching(format!("({start})-[{edge}]->({end})"));
                if !args.is_empty() {
                    let ids = id_values(args)?;
                    let condition = self.id_condition(&edge, ids);
                    self.filter(condition);
                }
                self.scope.extend([start.clone(), end.clone()]);
                self.enter(Current::Edge { name: edge, out_vertex: Some(start.clone()), in_vertex: Some(end), other: start });
            }
            "inject" => {
                let values = id_values(args)?;
                let parameter = self.parameter(Value::List(values));
                self.unwind(parameter);
            }
            "addV" => {
                let label = match args.first() {
                    None => "vertex".to_owned(),
                    arg => text_arg(arg, "addV()'s label")?,
                };
                let vertex = self.fresh("v");
                self.writing(format!("CREATE ({vertex}:{})", identifier(&label)));
                self.enter(Current::Vertex(vertex));
            }
            "addE" => {
                let label = text_arg(args.first(), "addE()'s label")?;
                let from = match &self.current {
                    Current::Vertex(name) => Some(name.clone()),
                    _ => None,
                };
                self.pending = Pending::Edge(PendingEdge { label, from, to: None, properties: Vec::new() });
            }
            "property" => {
                let element = self.element("property")?;
                let (key, value) = property_args(args)?;
                let parameter = self.parameter(value);
                self.writing(format!("SET {element}.{} = {parameter}", identifier(&key)));
            }
            "hasLabel" => {
                let labels = texts(args, "a label")?;
                let condition = self.label_condition(&labels)?;
                self.filter(condition);
            }
            "has" => self.has(args)?,
            "hasNot" => {
                let element = self.element("hasNot")?;
                let key = text_arg(args.first(), "hasNot()'s key")?;
                self.filter(format!("{element}.{} IS NULL", identifier(&key)));
            }
            "hasId" => {
                let element = self.element("hasId")?;
                let condition = match args {
                    [arg @ Arg::Predicate(_)] => self.test(&format!("id({element})"), arg)?,
                    _ => {
                        let ids = id_values(args)?;
                        self.id_condition(&element, ids)
                    }
                };
                self.filter(condition);
            }
            "is" => {
                let subject = self.expression("is")?;
                let test = args.first().ok_or("is() takes a value or a predicate")?;
                let condition = self.test(&subject, test)?;
                self.filter(condition);
            }
            "out" | "in" | "both" | "outE" | "inE" | "bothE" => self.navigate(step)?,
            "inV" | "outV" | "otherV" | "bothV" => self.edge_end(&step.name)?,
            "values" => {
                let element = self.element("values")?;
                let keys = texts(args, "a property key")?;
                if keys.is_empty() {
                    self.unwind(format!("[key IN keys({element}) | properties({element})[key]]"));
                } else {
                    let items = keys.iter().map(|key| format!("{element}.{}", identifier(key))).collect::<Vec<_>>().join(", ");
                    self.unwind(format!("[item IN [{items}] WHERE item IS NOT NULL]"));
                }
            }
            "id" => {
                let element = self.element("id")?;
                self.unwind(format!("[id({element})]"));
            }
            "label" => {
                let label = self.label_expression("label()")?;
                self.unwind(format!("[{label}]"));
            }
            "valueMap" => {
                self.element("valueMap")?;
                let tokens = matches!(args.first(), Some(Arg::Value(Value::Boolean(true))));
                let keys = texts(args.iter().filter(|arg| !matches!(arg, Arg::Value(Value::Boolean(_)))).cloned().collect::<Vec<_>>().as_slice(), "a property key")?;
                self.shape = Shape::ValueMap { tokens, keys };
                self.finished = true;
            }
            "elementMap" => {
                self.element("elementMap")?;
                self.shape = Shape::ElementMap { keys: texts(args, "a property key")? };
                self.finished = true;
            }
            "count" => {
                let expression = self.expression("count")?;
                self.reduce_to(format!("count({expression})"));
            }
            "sum" | "max" | "min" | "mean" => {
                let expression = self.expression(&step.name)?;
                let function = if step.name == "mean" { "avg" } else { step.name.as_str() };
                self.reduce_to(format!("{function}({expression})"));
            }
            "fold" => {
                let expression = self.expression("fold")?;
                self.reduce_to(format!("collect({expression})"));
            }
            "unfold" => {
                let expression = self.expression("unfold")?;
                self.unwind(expression);
            }
            "limit" | "skip" | "range" => {
                let scope = self.scope_list();
                let (skip, limit) = match step.name.as_str() {
                    "limit" => (0, count_arg(args.last(), "limit()'s count")?),
                    "skip" => (count_arg(args.last(), "skip()'s count")?, -1),
                    _ => {
                        let low = count_arg(args.first(), "range()'s start")?;
                        let high = count_arg(args.get(1), "range()'s end")?;
                        (low, if high < 0 { -1 } else { (high - low).max(0) })
                    }
                };
                let mut text = format!("WITH {scope}");
                if skip > 0 {
                    text.push_str(&format!(" SKIP {skip}"));
                }
                if limit >= 0 {
                    text.push_str(&format!(" LIMIT {limit}"));
                }
                self.clause(text);
                self.after_write = false;
            }
            "dedup" => {
                let expression = self.expression("dedup")?;
                self.clause(format!("WITH DISTINCT {expression}"));
                self.after_write = false;
                self.scope = vec![expression.clone()];
                self.path = vec![expression];
                self.labels.clear();
            }
            "order" => self.pending = Pending::Order(Vec::new()),
            "groupCount" => self.pending = Pending::GroupCount(None),
            "as" => {
                for label in texts(args, "a step label")? {
                    self.labels.push((label, self.current.clone()));
                }
            }
            "select" => {
                let labels = texts(args, "a step label")?;
                if labels.is_empty() {
                    return Err("select() names step labels".into());
                }
                self.pending = Pending::Select(labels, Vec::new());
            }
            "path" => {
                let items = self.path.join(", ");
                self.unwind(format!("[[{items}]]"));
                self.shape = Shape::Path;
                self.finished = true;
            }
            "constant" => {
                let value = match args.first() {
                    Some(Arg::Value(value)) => value.clone(),
                    _ => return Err("constant() takes a value".into()),
                };
                let parameter = self.parameter(value);
                self.unwind(format!("[{parameter}]"));
            }
            "drop" => {
                let element = self.element("drop")?;
                let detach = matches!(self.current, Current::Vertex(_));
                self.writing(format!("{}DELETE {element}", if detach { "DETACH " } else { "" }));
                self.shape = Shape::Nothing;
                self.finished = true;
            }
            "identity" | "barrier" => {}
            "where" | "not" | "and" | "or" | "union" | "coalesce" | "optional" | "repeat" => self.branch(step)?,
            other => return Err(unsupported(other)),
        }
        Ok(())
    }
}

fn property_args(args: &[Arg]) -> Compiling<(String, Value)> {
    let args = match args {
        [Arg::Token(_), rest @ ..] => rest,
        _ => args,
    };
    match args {
        [Arg::Token(token), _] if token == "id" || token == "label" => {
            Err("EggDB assigns element ids, and labels are set by addV() and addE()".into())
        }
        [Arg::Value(Value::String(key)), Arg::Value(value)] => Ok((key.clone(), value.clone())),
        _ => Err("property() takes a key and a value".into()),
    }
}

/// The Cypher query a traversal stands for.
pub(crate) fn compile(steps: &[Step]) -> Compiling<Compiled> {
    let mut compiler = Compiler {
        clauses: Vec::new(),
        parameters: Vec::new(),
        scope: Vec::new(),
        path: Vec::new(),
        labels: Vec::new(),
        current: Current::Start,
        counter: 0,
        after_write: false,
        open_match: None,
        pending: Pending::None,
        shape: Shape::Plain,
        writes: false,
        finished: false,
    };
    for step in steps {
        compiler.step(step)?;
    }
    compiler.flush()?;
    match (&compiler.shape, &compiler.current) {
        (Shape::Nothing, _) => {}
        (Shape::GroupCount, _) => compiler.clauses.push("RETURN gk, gc".into()),
        (_, Current::Start) => return Err("the traversal reaches nothing".into()),
        (_, current) => {
            let name = current.name().unwrap_or_default().to_owned();
            compiler.clauses.push(format!("RETURN {name}"));
        }
    }
    Ok(Compiled { query: compiler.clauses.join(" "), parameters: compiler.parameters, shape: compiler.shape, writes: compiler.writes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traversal::parse_script;

    fn cypher(script: &str) -> Compiled {
        compile(&parse_script(script, &[]).unwrap()).unwrap()
    }

    #[test]
    fn traversals_become_cypher() {
        let compiled = cypher("g.V().has('person', 'name', 'marko').out('knows').values('name')");
        assert_eq!(
            compiled.query,
            "MATCH (v1) WHERE (v1:person) AND v1.name = $p0 MATCH (v1)-[:knows]->(v2) UNWIND [item IN [v2.name] WHERE item IS NOT NULL] AS gv3 RETURN gv3"
        );
        assert_eq!(compiled.parameters, vec![("p0".to_owned(), Value::String("marko".into()))]);
        let created = cypher("g.addV('person').property('name', 'ada').as('a').addV('person').as('b').addE('knows').from('a').to('b')");
        assert!(created.writes);
        assert_eq!(
            created.query,
            "CREATE (v1:person) SET v1.name = $p0 CREATE (v2:person) CREATE (v1)-[e3:knows]->(v2) RETURN e3"
        );
        assert_eq!(cypher("g.V().count()").query, "MATCH (v1) WITH count(v1) AS gv2 RETURN gv2");
        assert_eq!(cypher("g.V().groupCount().by(label)").shape, Shape::GroupCount);
        assert_eq!(cypher("g.V(1).drop()").query, "MATCH (v1) WHERE id(v1) IN $p0 DETACH DELETE v1");
        assert!(compile(&parse_script("g.V().repeat(out()).times(2)", &[]).unwrap()).is_err());
    }
}
