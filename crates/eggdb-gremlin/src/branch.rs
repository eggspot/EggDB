use crate::compile::{Compiler, Compiling, Current, Pending, count_arg, id_values, identifier, text_arg, texts, type_list, unsupported};
use crate::traversal::{Arg, Step};
use eggdb_graph::Value;

const MOST_REPETITIONS: i64 = 15;

/// A `repeat()` of one navigation step, with its `times()`, `emit()` and `until()` modulators.
pub(crate) struct Repeat {
    navigation: Step,
    times: Option<i64>,
    emit: bool,
    until: Option<Vec<Step>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ends {
    Vertex,
    Edge,
    Value,
}

struct Branch {
    pattern: String,
    conditions: Vec<String>,
    current: String,
    kind: Ends,
    projection: Option<String>,
    edge_ends: Option<(Option<String>, Option<String>)>,
}

fn arrow(direction: &str, inner: &str) -> String {
    match direction {
        "out" | "outE" => format!("-[{inner}]->"),
        "in" | "inE" => format!("<-[{inner}]-"),
        _ => format!("-[{inner}]-"),
    }
}

impl Compiler {
    fn current_kind(&self) -> Compiling<(String, Ends)> {
        match &self.current {
            Current::Vertex(name) => Ok((name.clone(), Ends::Vertex)),
            Current::Edge { name, .. } => Ok((name.clone(), Ends::Edge)),
            Current::Value(name) => Ok((name.clone(), Ends::Value)),
            Current::Start => Err("a branch needs a traversal before it".into()),
        }
    }

    fn branch_condition(&mut self, branch: &Branch, step: &Step) -> Compiling<String> {
        let subject = branch.projection.clone().unwrap_or_else(|| branch.current.clone());
        let element = &branch.current;
        match (step.name.as_str(), step.args.as_slice()) {
            ("hasLabel", args) => {
                let labels = texts(args, "a label")?;
                Ok(match branch.kind {
                    Ends::Vertex => format!("({})", labels.iter().map(|label| format!("{element}:{}", identifier(label))).collect::<Vec<_>>().join(" OR ")),
                    _ => {
                        let parameter = self.parameter(Value::List(labels.into_iter().map(Value::String).collect()));
                        format!("type({element}) IN {parameter}")
                    }
                })
            }
            ("has", [Arg::Value(Value::String(key))]) => Ok(format!("{element}.{} IS NOT NULL", identifier(key))),
            ("has", [Arg::Value(Value::String(key)), test]) => self.test(&format!("{element}.{}", identifier(key)), test),
            ("has", [Arg::Value(Value::String(label)), Arg::Value(Value::String(key)), test]) => {
                let tested = self.test(&format!("{element}.{}", identifier(key)), test)?;
                Ok(format!("{element}:{} AND {tested}", identifier(label)))
            }
            ("hasNot", [Arg::Value(Value::String(key))]) => Ok(format!("{element}.{} IS NULL", identifier(key))),
            ("hasId", args) => {
                let parameter = self.parameter(Value::List(id_values(args)?));
                Ok(format!("id({element}) IN {parameter}"))
            }
            ("is", [test]) => self.test(&subject, test),
            _ => Err(format!("{}() cannot be used inside a branch in EggDB yet", step.name)),
        }
    }

    fn branch_list(&mut self, steps: &[Step]) -> Compiling<(String, Ends)> {
        let (start, start_kind) = self.current_kind()?;
        let navigates = steps.iter().any(|step| matches!(step.name.as_str(), "out" | "in" | "both" | "outE" | "inE" | "bothE"));
        if navigates && start_kind != Ends::Vertex {
            return Err("a branch that walks edges starts from vertices".into());
        }
        let alias = if navigates { start.clone() } else { self.fresh("y") };
        let mut branch = Branch {
            pattern: if navigates { format!("({start})") } else { String::new() },
            conditions: Vec::new(),
            current: alias.clone(),
            kind: start_kind,
            projection: None,
            edge_ends: None,
        };
        for step in steps {
            match step.name.as_str() {
                "out" | "in" | "both" => {
                    if branch.kind != Ends::Vertex {
                        return Err(format!("{}() inside a branch follows a vertex", step.name));
                    }
                    let next = self.fresh("w");
                    let types = type_list(&texts(&step.args, "an edge label")?);
                    branch.pattern.push_str(&format!("{}({next})", arrow(&step.name, &types)));
                    branch.current = next;
                }
                "outE" | "inE" | "bothE" => {
                    if branch.kind != Ends::Vertex {
                        return Err(format!("{}() inside a branch follows a vertex", step.name));
                    }
                    let edge = self.fresh("r");
                    let other = self.fresh("w");
                    let types = type_list(&texts(&step.args, "an edge label")?);
                    branch.pattern.push_str(&format!("{}({other})", arrow(&step.name, &format!("{edge}{types}"))));
                    let near = branch.current.clone();
                    branch.edge_ends = Some(match step.name.as_str() {
                        "outE" => (Some(near), Some(other.clone())),
                        "inE" => (Some(other.clone()), Some(near)),
                        _ => (None, Some(other.clone())),
                    });
                    branch.current = edge;
                    branch.kind = Ends::Edge;
                }
                "inV" | "outV" | "otherV" => {
                    let Some((out_vertex, in_vertex)) = branch.edge_ends.take() else {
                        return Err(format!("{}() inside a branch follows outE(), inE() or bothE()", step.name));
                    };
                    let vertex = match step.name.as_str() {
                        "inV" => in_vertex,
                        "outV" => out_vertex,
                        _ => in_vertex.or(out_vertex),
                    };
                    branch.current = vertex.ok_or("after bothE() inside a branch, use otherV()")?;
                    branch.kind = Ends::Vertex;
                }
                "values" => {
                    let key = text_arg(step.args.first(), "values()'s key inside a branch")?;
                    let expression = format!("{}.{}", branch.current, identifier(&key));
                    branch.conditions.push(format!("{expression} IS NOT NULL"));
                    branch.projection = Some(expression);
                    branch.kind = Ends::Value;
                }
                "id" => {
                    branch.projection = Some(format!("id({})", branch.current));
                    branch.kind = Ends::Value;
                }
                "label" => {
                    branch.projection = Some(match branch.kind {
                        Ends::Vertex => format!("head(labels({}))", branch.current),
                        _ => format!("type({})", branch.current),
                    });
                    branch.kind = Ends::Value;
                }
                "constant" => {
                    let Some(Arg::Value(value)) = step.args.first() else {
                        return Err("constant() takes a value".into());
                    };
                    branch.projection = Some(self.parameter(value.clone()));
                    branch.kind = Ends::Value;
                }
                "identity" => {}
                _ => {
                    let condition = self.branch_condition(&branch, step)?;
                    branch.conditions.push(condition);
                }
            }
        }
        let projection = branch.projection.clone().unwrap_or_else(|| branch.current.clone());
        let filter = if branch.conditions.is_empty() { String::new() } else { format!(" WHERE {}", branch.conditions.join(" AND ")) };
        let list = if navigates {
            format!("[{}{filter} | {projection}]", branch.pattern)
        } else {
            format!("[{alias} IN [{start}]{filter} | {projection}]")
        };
        Ok((list, branch.kind))
    }

    fn traversal_args<'s>(&self, step: &'s Step) -> Compiling<Vec<&'s Vec<Step>>> {
        step.args
            .iter()
            .map(|arg| match arg {
                Arg::Traversal(steps) => Ok(steps),
                _ => Err(format!("{}() takes anonymous traversals such as __.out('knows')", step.name)),
            })
            .collect()
    }

    fn unwind_as(&mut self, list: String, kind: Ends) {
        self.unwind(list);
        let Current::Value(name) = self.current.clone() else {
            return;
        };
        self.current = match kind {
            Ends::Vertex => Current::Vertex(name),
            Ends::Edge => Current::Edge { name, out_vertex: None, in_vertex: None, other: String::new() },
            Ends::Value => Current::Value(name),
        };
    }

    fn same_ends(kinds: &[Ends]) -> Ends {
        match kinds.first() {
            Some(first) if kinds.iter().all(|kind| kind == first) => *first,
            _ => Ends::Value,
        }
    }

    /// Compiles `where`, `not`, `and`, `or`, `union`, `coalesce`, `optional` and `repeat` (D1016).
    pub(crate) fn branch(&mut self, step: &Step) -> Compiling<()> {
        match step.name.as_str() {
            "where" => match step.args.as_slice() {
                [Arg::Predicate(predicate)] if matches!(predicate.name.as_str(), "eq" | "neq") => {
                    let Some(Value::String(label)) = predicate.values.first() else {
                        return Err("where() with eq or neq names a step label".into());
                    };
                    let other = self
                        .labels
                        .iter()
                        .find(|(name, _)| name == label)
                        .and_then(|(_, found)| found.name().map(str::to_owned))
                        .ok_or_else(|| format!("where() names no step label '{label}'"))?;
                    let current = self.expression("where")?;
                    let operator = if predicate.name == "eq" { "=" } else { "<>" };
                    self.filter(format!("{current} {operator} {other}"));
                }
                [Arg::Traversal(steps)] => {
                    let (list, _) = self.branch_list(steps)?;
                    self.filter(format!("size({list}) > 0"));
                }
                _ => return Err("where() takes an anonymous traversal, or P.eq or P.neq of a step label".into()),
            },
            "not" => {
                let [steps] = self.traversal_args(step)?[..] else {
                    return Err("not() takes one anonymous traversal".into());
                };
                let (list, _) = self.branch_list(steps)?;
                self.filter(format!("size({list}) = 0"));
            }
            "and" | "or" => {
                let mut parts = Vec::new();
                for steps in self.traversal_args(step)? {
                    let (list, _) = self.branch_list(steps)?;
                    parts.push(format!("size({list}) > 0"));
                }
                if parts.is_empty() {
                    return Err(format!("{}() takes anonymous traversals", step.name));
                }
                let joiner = if step.name == "and" { " AND " } else { " OR " };
                self.filter(format!("({})", parts.join(joiner)));
            }
            "union" => {
                let mut lists = Vec::new();
                let mut kinds = Vec::new();
                for steps in self.traversal_args(step)? {
                    let (list, kind) = self.branch_list(steps)?;
                    lists.push(list);
                    kinds.push(kind);
                }
                if lists.is_empty() {
                    return Err("union() takes anonymous traversals".into());
                }
                self.unwind_as(lists.join(" + "), Self::same_ends(&kinds));
            }
            "coalesce" | "optional" => {
                let mut lists = Vec::new();
                let mut kinds = Vec::new();
                for steps in self.traversal_args(step)? {
                    let (list, kind) = self.branch_list(steps)?;
                    lists.push(list);
                    kinds.push(kind);
                }
                if step.name == "optional" {
                    if lists.len() != 1 {
                        return Err("optional() takes one anonymous traversal".into());
                    }
                    let (current, kind) = self.current_kind()?;
                    lists.push(format!("[{current}]"));
                    kinds.push(kind);
                }
                if lists.is_empty() {
                    return Err("coalesce() takes anonymous traversals".into());
                }
                let mut chosen = "[]".to_owned();
                for list in lists.iter().rev() {
                    chosen = format!("CASE WHEN size({list}) > 0 THEN {list} ELSE {chosen} END");
                }
                self.unwind_as(chosen, Self::same_ends(&kinds));
            }
            "repeat" => {
                let [steps] = self.traversal_args(step)?[..] else {
                    return Err("repeat() takes one anonymous traversal".into());
                };
                let navigation = match steps.as_slice() {
                    [navigation] if matches!(navigation.name.as_str(), "out" | "in" | "both") => navigation.clone(),
                    _ => return Err("repeat() in EggDB repeats one out(), in() or both() step yet".into()),
                };
                self.vertex("repeat")?;
                self.pending = Pending::Repeat(Repeat { navigation, times: None, emit: false, until: None });
            }
            other => return Err(unsupported(other)),
        }
        Ok(())
    }

    pub(crate) fn modulate_repeat(&mut self, step: &Step) -> Compiling<()> {
        let Pending::Repeat(repeat) = &mut self.pending else {
            return Err(format!("{}() follows repeat()", step.name));
        };
        match step.name.as_str() {
            "times" => {
                let times = count_arg(step.args.first(), "times()'s count")?;
                if !(1..=MOST_REPETITIONS).contains(&times) {
                    return Err(format!("times() counts from 1 to {MOST_REPETITIONS}"));
                }
                repeat.times = Some(times);
            }
            "emit" if step.args.is_empty() => repeat.emit = true,
            "until" => match step.args.as_slice() {
                [Arg::Traversal(steps)] => repeat.until = Some(steps.clone()),
                _ => return Err("until() takes one anonymous traversal".into()),
            },
            _ => return Err(format!("{}() with arguments is not supported after repeat() yet", step.name)),
        }
        Ok(())
    }

    pub(crate) fn repeated(&mut self, repeat: Repeat) -> Compiling<()> {
        let from = self.vertex("repeat")?;
        let most = match (repeat.times, &repeat.until) {
            (Some(times), _) => times,
            (None, Some(_)) => MOST_REPETITIONS,
            (None, None) => return Err("repeat() needs times() or until() in EggDB, so the walk has an end".into()),
        };
        let least = if repeat.emit || repeat.until.is_some() { 1 } else { most };
        let types = type_list(&texts(&repeat.navigation.args, "an edge label")?);
        let to = self.fresh("v");
        let pattern = format!("({from}){}({to})", arrow(&repeat.navigation.name, &format!("{types}*{least}..{most}")));
        self.matching(pattern);
        self.enter(Current::Vertex(to));
        if let Some(until) = repeat.until {
            let (list, _) = self.branch_list(&until)?;
            self.filter(format!("size({list}) > 0"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::compile::compile;
    use crate::traversal::parse_script;

    fn cypher(script: &str) -> String {
        compile(&parse_script(script, &[]).unwrap()).unwrap().query
    }

    #[test]
    fn branching_steps_become_list_expressions_and_variable_length_matches() {
        assert_eq!(
            cypher("g.V().where(__.out('knows').has('age', P.gt(30))).values('name')"),
            "MATCH (v1) WHERE size([(v1)-[:knows]->(w2) WHERE w2.age > $p0 | w2]) > 0 UNWIND [item IN [v1.name] WHERE item IS NOT NULL] AS gv3 RETURN gv3"
        );
        assert_eq!(
            cypher("g.V(1).repeat(out('knows')).times(2)"),
            "MATCH (v1) WHERE id(v1) IN $p0 MATCH (v1)-[:knows*2..2]->(v2) RETURN v2"
        );
        assert!(cypher("g.V().union(__.out('a'), __.in('b')).count()").contains("UNWIND [(v1)-[:a]->(w2) | w2] + [(v1)<-[:b]-(w3) | w3] AS gv4"));
        assert!(cypher("g.V().coalesce(__.values('nick'), __.constant('none'))").contains("CASE WHEN size("));
        assert!(compile(&parse_script("g.V().repeat(out())", &[]).unwrap()).is_err());
    }
}
