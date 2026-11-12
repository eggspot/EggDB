//! SQL/PGQ property graphs over tables (D1007): `CREATE PROPERTY GRAPH` definitions, and
//! `GRAPH_TABLE (graph MATCH pattern [WHERE ...] COLUMNS (...))` rewritten, before the
//! statement is parsed, into joins of the vertex and edge tables the pattern's labels and
//! directions allow, one `SELECT` per combination, combined with `UNION ALL`.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, tokenize_in};

pub(crate) const GRAPHS_TABLE: &str = "eggdb_internal.property_graphs";
pub(crate) const CREATE_GRAPHS_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.property_graphs (name text PRIMARY KEY, definition text NOT NULL)";
const MOST_COMBINATIONS: usize = 64;
const MOST_PATH_ELEMENTS: usize = 41;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    columns: Vec<String>,
    vertex: String,
    vertex_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ElementTable {
    table: String,
    alias: String,
    key: Vec<String>,
    labels: Vec<String>,
    renamed: Vec<(String, String)>,
    source: Option<Reference>,
    destination: Option<Reference>,
}

/// A property graph's vertex and edge tables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PropertyGraph {
    pub(crate) name: String,
    vertices: Vec<ElementTable>,
    edges: Vec<ElementTable>,
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, message)
}

struct Reader<'t> {
    tokens: &'t [Token<'t>],
    at: usize,
}

impl Reader<'_> {
    fn word(&self) -> Option<String> {
        match &self.tokens.get(self.at)?.kind {
            TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
            TokenKind::QuotedIdentifier(name) => Some(name.clone()),
            _ => None,
        }
    }

    fn is_word(&self, expected: &str) -> bool {
        matches!(self.tokens.get(self.at).map(|token| &token.kind), Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case(expected))
    }

    fn take_word(&mut self, expected: &str) -> bool {
        let found = self.is_word(expected);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_word(&mut self, expected: &str) -> SqlResult<()> {
        if self.take_word(expected) {
            Ok(())
        } else {
            Err(invalid(format!("CREATE PROPERTY GRAPH expects {} here", expected.to_ascii_uppercase())))
        }
    }

    fn name(&mut self) -> SqlResult<String> {
        let mut name = self.word().ok_or_else(|| invalid("CREATE PROPERTY GRAPH expects a name here"))?;
        self.at += 1;
        while matches!(self.tokens.get(self.at).map(|token| &token.kind), Some(TokenKind::Dot)) {
            self.at += 1;
            let part = self.word().ok_or_else(|| invalid("a name ends with a dot"))?;
            self.at += 1;
            name = format!("{name}.{part}");
        }
        Ok(name)
    }

    fn take(&mut self, kind: &TokenKind<'_>) -> bool {
        let found = self.tokens.get(self.at).is_some_and(|token| &token.kind == kind);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, kind: &TokenKind<'_>, what: &str) -> SqlResult<()> {
        if self.take(kind) { Ok(()) } else { Err(invalid(format!("CREATE PROPERTY GRAPH expects {what} here"))) }
    }

    fn column_list(&mut self) -> SqlResult<Vec<String>> {
        self.expect(&TokenKind::LeftParen, "(")?;
        let mut columns = vec![self.name()?];
        while self.take(&TokenKind::Comma) {
            columns.push(self.name()?);
        }
        self.expect(&TokenKind::RightParen, ")")?;
        Ok(columns)
    }

    fn reference(&mut self) -> SqlResult<Reference> {
        self.expect_word("key")?;
        let columns = self.column_list()?;
        self.expect_word("references")?;
        let vertex = self.name()?;
        let vertex_columns = self.column_list()?;
        if columns.len() != vertex_columns.len() {
            return Err(invalid("a SOURCE or DESTINATION KEY and its REFERENCES list differ in length"));
        }
        Ok(Reference { columns, vertex, vertex_columns })
    }

    fn element(&mut self, edge: bool) -> SqlResult<ElementTable> {
        let table = self.name()?;
        let alias = if self.take_word("as") { self.name()? } else { table.rsplit('.').next().unwrap_or(&table).to_owned() };
        let mut element = ElementTable {
            table,
            alias,
            key: Vec::new(),
            labels: Vec::new(),
            renamed: Vec::new(),
            source: None,
            destination: None,
        };
        loop {
            if self.take_word("key") {
                element.key = self.column_list()?;
            } else if edge && self.take_word("source") {
                element.source = Some(self.reference()?);
            } else if edge && self.take_word("destination") {
                element.destination = Some(self.reference()?);
            } else if self.take_word("label") {
                element.labels.push(self.name()?);
            } else if self.take_word("default") {
                self.expect_word("label")?;
                element.labels.push(element.alias.clone());
            } else if self.take_word("no") {
                self.expect_word("properties")?;
            } else if self.take_word("properties") {
                if self.take_word("are") {
                    self.expect_word("all")?;
                    self.expect_word("columns")?;
                } else {
                    self.expect(&TokenKind::LeftParen, "(")?;
                    loop {
                        let column = self.name()?;
                        let property = if self.take_word("as") { self.name()? } else { column.clone() };
                        if property != column {
                            element.renamed.push((property, column));
                        }
                        if !self.take(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RightParen, ")")?;
                }
            } else {
                break;
            }
        }
        if element.labels.is_empty() {
            element.labels.push(element.alias.clone());
        }
        if edge && (element.source.is_none() || element.destination.is_none()) {
            return Err(invalid(format!("the edge table {} needs SOURCE KEY and DESTINATION KEY", element.alias)));
        }
        Ok(element)
    }

    fn elements(&mut self, edge: bool) -> SqlResult<Vec<ElementTable>> {
        self.expect(&TokenKind::LeftParen, "(")?;
        let mut elements = vec![self.element(edge)?];
        while self.take(&TokenKind::Comma) {
            elements.push(self.element(edge)?);
        }
        self.expect(&TokenKind::RightParen, ")")?;
        Ok(elements)
    }
}

/// Whether `text` starts a statement this module handles: `CREATE [OR REPLACE] PROPERTY
/// GRAPH` or `DROP PROPERTY GRAPH`.
pub(crate) fn graph_command(text: &str) -> Option<bool> {
    let lower = text.trim_start().to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().take(5).collect();
    match words.as_slice() {
        ["create", "property", "graph", ..] | ["create", "or", "replace", "property", "graph"] => Some(true),
        ["drop", "property", "graph", ..] => Some(false),
        _ => None,
    }
}

/// Reads a `CREATE [OR REPLACE] PROPERTY GRAPH` statement.
pub(crate) fn parse_definition(text: &str) -> SqlResult<(PropertyGraph, bool)> {
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let mut reader = Reader { tokens: &tokens, at: 0 };
    reader.expect_word("create")?;
    let replace = reader.take_word("or");
    if replace {
        reader.expect_word("replace")?;
    }
    reader.expect_word("property")?;
    reader.expect_word("graph")?;
    let name = reader.name()?;
    reader.expect_word("vertex")?;
    reader.expect_word("tables")?;
    let vertices = reader.elements(false)?;
    let edges = if reader.take_word("edge") {
        reader.expect_word("tables")?;
        reader.elements(true)?
    } else {
        Vec::new()
    };
    for edge in &edges {
        for reference in [&edge.source, &edge.destination].into_iter().flatten() {
            if !vertices.iter().any(|vertex| vertex.alias == reference.vertex || vertex.table == reference.vertex) {
                return Err(invalid(format!("the edge table {} references {}, which is not a vertex table", edge.alias, reference.vertex)));
            }
        }
    }
    Ok((PropertyGraph { name, vertices, edges }, replace))
}

/// The graph a `DROP PROPERTY GRAPH [IF EXISTS] name` names, and whether `IF EXISTS` was given.
pub(crate) fn dropped_graph(text: &str) -> SqlResult<(String, bool)> {
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let mut reader = Reader { tokens: &tokens, at: 0 };
    reader.expect_word("drop")?;
    reader.expect_word("property")?;
    reader.expect_word("graph")?;
    let if_exists = reader.take_word("if");
    if if_exists {
        reader.expect_word("exists")?;
    }
    Ok((reader.name()?, if_exists))
}

#[derive(Debug, Clone, Default)]
struct ElementPattern {
    variable: Option<String>,
    labels: Vec<String>,
    condition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
    Either,
}

fn element_pattern(text: &str) -> ElementPattern {
    let text = text.trim();
    let (head, condition) = match find_word(text, "where") {
        Some(at) => (&text[..at], Some(text[at + 5..].trim().to_owned())),
        None => (text, None),
    };
    let head = head.trim();
    let (variable, labels) = if let Some(at) = find_word(head, "is") {
        (head[..at].trim(), Some(head[at + 2..].trim()))
    } else if let Some((variable, labels)) = head.split_once(':') {
        (variable.trim(), Some(labels.trim()))
    } else {
        (head, None)
    };
    ElementPattern {
        variable: (!variable.is_empty()).then(|| variable.to_ascii_lowercase()),
        labels: labels
            .map(|labels| labels.split('|').map(|label| label.trim().to_ascii_lowercase()).filter(|label| !label.is_empty()).collect())
            .unwrap_or_default(),
        condition,
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn find_word(text: &str, word: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    let mut quoted = false;
    let mut depth = 0usize;
    while from < bytes.len() {
        match bytes[from] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth = depth.saturating_sub(1),
            _ => {}
        }
        if !quoted
            && depth == 0
            && lower[from..].starts_with(word)
            && (from == 0 || !is_word_byte(bytes[from - 1]))
            && bytes.get(from + word.len()).is_none_or(|byte| !is_word_byte(*byte))
        {
            return Some(from);
        }
        from += 1;
    }
    None
}

const MOST_REPETITIONS: usize = 10;

#[derive(Debug, Clone)]
struct Path {
    nodes: Vec<ElementPattern>,
    edges: Vec<(ElementPattern, Direction)>,
}

#[derive(Debug, Clone)]
struct EdgeStep {
    pattern: ElementPattern,
    direction: Direction,
    least: usize,
    most: usize,
    quantified: bool,
}

fn split_paths(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut start = 0;
    for (at, byte) in text.bytes().enumerate() {
        match byte {
            b'\'' => quoted = !quoted,
            b'(' | b'[' | b'{' if !quoted => depth += 1,
            b')' | b']' | b'}' if !quoted => depth = depth.saturating_sub(1),
            b',' if !quoted && depth == 0 => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn rebound(condition: &str, from: &str, to: &str) -> String {
    let lower = condition.to_ascii_lowercase();
    let needle = format!("{from}.");
    let mut out = String::with_capacity(condition.len());
    let mut at = 0;
    while let Some(found) = lower.get(at..).and_then(|rest| rest.find(&needle)) {
        let start = at + found;
        let end = start + needle.len();
        out.push_str(&condition[at..start]);
        if start == 0 || !is_word_byte(lower.as_bytes()[start - 1]) {
            out.push_str(&format!("{to}."));
        } else {
            out.push_str(&condition[start..end]);
        }
        at = end;
    }
    out.push_str(&condition[at..]);
    out
}

fn path(text: &str) -> SqlResult<Vec<Path>> {
    let bytes = text.as_bytes();
    let mut nodes = Vec::new();
    let mut steps = Vec::new();
    let mut at = 0;
    let mut between = 0;
    let mut in_edge = false;
    while at < bytes.len() {
        if bytes[at] == b'[' && !nodes.is_empty() {
            in_edge = true;
        } else if bytes[at] == b']' {
            in_edge = false;
        } else if bytes[at] == b'(' && !in_edge {
            let mut depth = 0usize;
            let start = at;
            while at < bytes.len() {
                match bytes[at] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            if at >= bytes.len() {
                return Err(invalid("an element pattern is not closed"));
            }
            let inner = text.get(start + 1..at).ok_or_else(|| invalid("an element pattern is not closed"))?;
            if !nodes.is_empty() {
                steps.push(edge_step(text.get(between..start).unwrap_or_default().trim())?);
            }
            between = at + 1;
            nodes.push(element_pattern(inner));
            if nodes.len() > MOST_PATH_ELEMENTS {
                return Err(invalid("a path pattern of more than 40 edges"));
            }
        } else if nodes.is_empty() && !bytes[at].is_ascii_whitespace() {
            return Err(invalid("a path pattern starts with a node in parentheses"));
        }
        at += 1;
    }
    if nodes.is_empty() || !text.get(between..).unwrap_or_default().trim().is_empty() {
        return Err(invalid("a path pattern starts and ends with a node in parentheses"));
    }
    let mut expanded = vec![Path { nodes: vec![nodes[0].clone()], edges: Vec::new() }];
    for (index, step) in steps.iter().enumerate() {
        let mut grown = Vec::new();
        for partial in &expanded {
            for length in step.least..=step.most {
                let mut path = partial.clone();
                for repetition in 1..=length {
                    let mut pattern = step.pattern.clone();
                    if step.quantified
                        && let Some(variable) = pattern.variable.clone()
                    {
                        let named = format!("{variable}_{repetition}");
                        pattern.condition = pattern.condition.map(|condition| rebound(&condition, &variable, &named));
                        pattern.variable = Some(named);
                    }
                    path.edges.push((pattern, step.direction));
                    path.nodes.push(if repetition == length { nodes[index + 1].clone() } else { ElementPattern::default() });
                }
                if path.edges.len() + 1 > MOST_PATH_ELEMENTS {
                    return Err(invalid("a path pattern of more than 40 edges"));
                }
                grown.push(path);
                if grown.len() > MOST_COMBINATIONS {
                    return Err(too_many_combinations());
                }
            }
        }
        expanded = grown;
    }
    Ok(expanded)
}

fn too_many_combinations() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a GRAPH_TABLE pattern stands for more than {MOST_COMBINATIONS} combinations of tables; name labels to narrow it"),
    )
}

fn quantifier(compact: &str) -> SqlResult<(&str, Option<(usize, usize)>)> {
    if compact.ends_with('+') || compact.ends_with('*') {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "a quantified edge in GRAPH_TABLE needs an upper bound, such as {1,5}",
        ));
    }
    let Some(open) = compact.strip_suffix('}').and_then(|inner| inner.rfind('{')) else {
        return Ok((compact, None));
    };
    let inside = &compact[open + 1..compact.len() - 1];
    let number = |text: &str| text.parse::<usize>().map_err(|_| invalid(format!("the quantifier {{{inside}}} is not a pair of counts")));
    let (least, most) = match inside.split_once(',') {
        Some((least, most)) if !most.is_empty() => (if least.is_empty() { 1 } else { number(least)? }, number(most)?),
        Some(_) => {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a quantified edge in GRAPH_TABLE needs an upper bound, such as {1,5}",
            ));
        }
        None => (number(inside)?, number(inside)?),
    };
    if least == 0 {
        return Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "a quantified edge in GRAPH_TABLE repeats at least once"));
    }
    if least > most || most > MOST_REPETITIONS {
        return Err(invalid(format!("a quantifier counts from 1 up to {MOST_REPETITIONS}, the smaller count first")));
    }
    Ok((&compact[..open], Some((least, most))))
}

fn edge_step(text: &str) -> SqlResult<EdgeStep> {
    let compact: String = text.chars().filter(|character| !character.is_whitespace()).collect();
    let (arrow, counts) = quantifier(&compact)?;
    let backward = arrow.starts_with("<-");
    let forward = arrow.ends_with("->");
    let direction = match (backward, forward) {
        (true, false) => Direction::Backward,
        (false, true) => Direction::Forward,
        (false, false) => Direction::Either,
        (true, true) => return Err(invalid("an edge pattern points both ways")),
    };
    let pattern = match (text.find('['), text.rfind(']')) {
        (Some(open), Some(close)) if open < close => element_pattern(&text[open + 1..close]),
        _ => ElementPattern::default(),
    };
    let (least, most) = counts.unwrap_or((1, 1));
    Ok(EdgeStep { pattern, direction, least, most, quantified: counts.is_some() })
}

impl PropertyGraph {
    fn vertex(&self, name: &str) -> Option<&ElementTable> {
        self.vertices.iter().find(|vertex| vertex.alias == name || vertex.table == name)
    }

    fn matches(element: &ElementTable, pattern: &ElementPattern) -> bool {
        pattern.labels.is_empty() || pattern.labels.iter().any(|label| element.labels.contains(label))
    }
}

#[derive(Clone)]
struct Combination<'g> {
    nodes: Vec<&'g ElementTable>,
    edges: Vec<(&'g ElementTable, bool)>,
}

type Bound<'g> = Vec<(String, &'g ElementTable)>;

fn allowed(bound: &Bound<'_>, variable: &str, element: &ElementTable) -> bool {
    bound.iter().all(|(name, found)| name != variable || std::ptr::eq(*found, element))
}

fn with_variables<'g>(bound: &Bound<'g>, path: &Path, names: &PathNames, combination: &Combination<'g>) -> Bound<'g> {
    let mut out = bound.clone();
    for (index, &node) in combination.nodes.iter().enumerate() {
        if path.nodes[index].variable.is_some() && !out.iter().any(|(name, _)| *name == names.nodes[index]) {
            out.push((names.nodes[index].clone(), node));
        }
    }
    for (index, &(edge, _)) in combination.edges.iter().enumerate() {
        if path.edges[index].0.variable.is_some() && !out.iter().any(|(name, _)| *name == names.edges[index]) {
            out.push((names.edges[index].clone(), edge));
        }
    }
    out
}

fn combinations<'g>(graph: &'g PropertyGraph, path: &Path, names: &PathNames, bound: &Bound<'g>) -> SqlResult<Vec<Combination<'g>>> {
    let fits_node = |index: usize, vertex: &ElementTable, partial: &Combination<'_>| {
        PropertyGraph::matches(vertex, &path.nodes[index])
            && (path.nodes[index].variable.is_none()
                || (allowed(bound, &names.nodes[index], vertex)
                    && (0..index).all(|earlier| names.nodes[earlier] != names.nodes[index] || std::ptr::eq(partial.nodes[earlier], vertex))))
    };
    let mut partial: Vec<Combination<'g>> = Vec::new();
    for first in &graph.vertices {
        let empty = Combination { nodes: Vec::new(), edges: Vec::new() };
        if fits_node(0, first, &empty) {
            partial.push(Combination { nodes: vec![first], edges: Vec::new() });
        }
    }
    for (index, (edge_pattern, direction)) in path.edges.iter().enumerate() {
        let mut grown = Vec::new();
        for combination in &partial {
            let Some(&from) = combination.nodes.last() else {
                continue;
            };
            for edge in graph.edges.iter().filter(|edge| PropertyGraph::matches(edge, edge_pattern)) {
                if edge_pattern.variable.is_some() && !allowed(bound, &names.edges[index], edge) {
                    continue;
                }
                let orientations: &[bool] = match direction {
                    Direction::Forward => &[true],
                    Direction::Backward => &[false],
                    Direction::Either => &[true, false],
                };
                for &forward in orientations {
                    let (near, far) = if forward { (&edge.source, &edge.destination) } else { (&edge.destination, &edge.source) };
                    let (Some(near), Some(far)) = (near, far) else {
                        continue;
                    };
                    let near_matches = graph.vertex(&near.vertex).is_some_and(|vertex| vertex.alias == from.alias);
                    let Some(to) = graph.vertex(&far.vertex) else {
                        continue;
                    };
                    if near_matches && fits_node(index + 1, to, combination) {
                        if grown.len() >= MOST_COMBINATIONS {
                            return Err(too_many_combinations());
                        }
                        let mut next = combination.clone();
                        next.nodes.push(to);
                        next.edges.push((edge, forward));
                        grown.push(next);
                    }
                }
            }
        }
        partial = grown;
    }
    Ok(partial)
}

struct PathNames {
    nodes: Vec<String>,
    edges: Vec<String>,
}

fn path_names(path: &Path, anonymous: &mut usize) -> PathNames {
    let mut next = |variable: &Option<String>, kind: &str| {
        variable.clone().unwrap_or_else(|| {
            *anonymous += 1;
            format!("eggdb_{kind}_{anonymous}")
        })
    };
    let nodes = path.nodes.iter().map(|node| next(&node.variable, "node")).collect();
    let edges: Vec<String> = path.edges.iter().map(|(edge, _)| next(&edge.variable, "edge")).collect();
    PathNames { nodes, edges }
}

fn joined(left: &str, left_columns: &[String], right: &str, right_columns: &[String]) -> String {
    left_columns
        .iter()
        .zip(right_columns)
        .map(|(left_column, right_column)| format!("{left}.{left_column} = {right}.{right_column}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn renamed(text: &str, variables: &[(String, &ElementTable)]) -> String {
    let mut out = text.to_owned();
    for (variable, element) in variables {
        for (property, column) in &element.renamed {
            out = replace_reference(&out, variable, property, column);
        }
    }
    out
}

fn replace_reference(text: &str, variable: &str, property: &str, column: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let needle = format!("{variable}.{property}");
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower.get(at..).and_then(|rest| rest.find(&needle)) {
        let start = at + found;
        let end = start + needle.len();
        let bounded = (start == 0 || !is_word_byte(lower.as_bytes()[start - 1]))
            && lower.as_bytes().get(end).is_none_or(|byte| !is_word_byte(*byte));
        out.push_str(&text[at..start]);
        if bounded {
            out.push_str(&format!("{variable}.{column}"));
        } else {
            out.push_str(&text[start..end]);
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

fn from_clause<'g>(paths: &[(&PathNames, &Combination<'g>)]) -> SqlResult<(String, Vec<(String, &'g ElementTable)>)> {
    let mut from = String::new();
    let mut variables: Vec<(String, &'g ElementTable)> = Vec::new();
    for (names, combination) in paths {
        let first = &names.nodes[0];
        if !variables.iter().any(|(name, _)| name == first) {
            if !from.is_empty() {
                from.push_str(" CROSS JOIN ");
            }
            from.push_str(&format!("{} AS {first}", combination.nodes[0].table));
            variables.push((first.clone(), combination.nodes[0]));
        }
        for (index, &(edge, forward)) in combination.edges.iter().enumerate() {
            let (near, far) = if forward { (&edge.source, &edge.destination) } else { (&edge.destination, &edge.source) };
            let (Some(near), Some(far)) = (near, far) else {
                continue;
            };
            let edge_alias = &names.edges[index];
            if variables.iter().any(|(name, _)| name == edge_alias) {
                return Err(SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("the edge variable {edge_alias} appears twice in a GRAPH_TABLE pattern"),
                ));
            }
            let to_alias = &names.nodes[index + 1];
            let near_condition = joined(edge_alias, &near.columns, &names.nodes[index], &near.vertex_columns);
            let far_condition = joined(edge_alias, &far.columns, to_alias, &far.vertex_columns);
            variables.push((edge_alias.clone(), edge));
            if variables.iter().any(|(name, _)| name == to_alias) {
                from.push_str(&format!(" JOIN {} AS {edge_alias} ON {near_condition} AND {far_condition}", edge.table));
            } else {
                from.push_str(&format!(
                    " JOIN {} AS {edge_alias} ON {near_condition} JOIN {} AS {to_alias} ON {far_condition}",
                    edge.table,
                    combination.nodes[index + 1].table,
                ));
                variables.push((to_alias.clone(), combination.nodes[index + 1]));
            }
        }
    }
    Ok((from, variables))
}

fn path_conditions(path: &Path) -> impl Iterator<Item = String> + '_ {
    path.nodes
        .iter()
        .filter_map(|node| node.condition.clone())
        .chain(path.edges.iter().filter_map(|(edge, _)| edge.condition.clone()))
}

/// The query a `GRAPH_TABLE` body (`graph MATCH pattern [, pattern ...] [WHERE ...] COLUMNS
/// (...)`) stands for over `graph`.
pub(crate) fn graph_table_query(graph: &PropertyGraph, body: &str) -> SqlResult<String> {
    let match_at = find_word(body, "match").ok_or_else(|| invalid("GRAPH_TABLE needs MATCH"))?;
    let rest = &body[match_at + 5..];
    let columns_at = find_word(rest, "columns").ok_or_else(|| invalid("GRAPH_TABLE needs COLUMNS (...)"))?;
    let (pattern_text, filter) = match find_word(&rest[..columns_at], "where") {
        Some(at) => (&rest[..at], Some(rest[at + 5..columns_at].trim())),
        None => (&rest[..columns_at], None),
    };
    let columns_text = rest[columns_at + 7..].trim();
    let columns = columns_text
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .ok_or_else(|| invalid("COLUMNS lists its columns in parentheses"))?;
    let mut instances: Vec<Vec<Path>> = vec![Vec::new()];
    for text in split_paths(pattern_text) {
        let alternatives = path(text.trim())?;
        let mut grown = Vec::new();
        for instance in &instances {
            for alternative in &alternatives {
                let mut next = instance.clone();
                next.push(alternative.clone());
                grown.push(next);
                if grown.len() > MOST_COMBINATIONS {
                    return Err(too_many_combinations());
                }
            }
        }
        instances = grown;
    }
    let mut selects = Vec::new();
    for instance in &instances {
        let mut anonymous = 0usize;
        let names: Vec<PathNames> = instance.iter().map(|path| path_names(path, &mut anonymous)).collect();
        let mut assignments: Vec<(Vec<Combination<'_>>, Bound<'_>)> = vec![(Vec::new(), Vec::new())];
        for (path, path_names) in instance.iter().zip(&names) {
            let mut grown = Vec::new();
            for (chosen, bound) in &assignments {
                for combination in combinations(graph, path, path_names, bound)? {
                    let bound = with_variables(bound, path, path_names, &combination);
                    let mut chosen = chosen.clone();
                    chosen.push(combination);
                    grown.push((chosen, bound));
                    if grown.len() + selects.len() > MOST_COMBINATIONS {
                        return Err(too_many_combinations());
                    }
                }
            }
            assignments = grown;
        }
        for (chosen, _) in assignments {
            let parts: Vec<(&PathNames, &Combination<'_>)> = names.iter().zip(&chosen).collect();
            let (from, variables) = from_clause(&parts)?;
            let mut conditions: Vec<String> = instance.iter().flat_map(path_conditions).collect();
            conditions.extend(filter.map(str::to_owned));
            let filter = if conditions.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conditions.iter().map(|condition| format!("({condition})")).collect::<Vec<_>>().join(" AND "))
            };
            selects.push(renamed(&format!("SELECT {columns} FROM {from}{filter}"), &variables));
        }
    }
    if selects.is_empty() {
        return Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("no tables of the property graph {} match the pattern's labels and directions", graph.name),
        ));
    }
    Ok(format!("({})", selects.join(" UNION ALL ")))
}

/// The `GRAPH_TABLE (...)` spans of a statement: each one's byte range, its graph's name and
/// its body after the name.
pub(crate) fn graph_tables(text: &str) -> SqlResult<Vec<(std::ops::Range<usize>, String, String)>> {
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let mut spans = Vec::new();
    let mut at = 0;
    while at + 2 < tokens.len() {
        let is_graph_table = matches!(&tokens[at].kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("graph_table"))
            && tokens[at + 1].kind == TokenKind::LeftParen;
        if !is_graph_table {
            at += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut close = None;
        for (index, token) in tokens.iter().enumerate().skip(at + 1) {
            match token.kind {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.ok_or_else(|| invalid("GRAPH_TABLE is not closed"))?;
        let name_token = &tokens[at + 2];
        let name = text.get(name_token.start..name_token.end).unwrap_or_default().to_ascii_lowercase();
        let body = text.get(name_token.end..tokens[close].start).unwrap_or_default().to_owned();
        spans.push((tokens[at].start..tokens[close].end, name, body));
        at = close + 1;
    }
    Ok(spans)
}

const MOST_ALGORITHM_ROWS: usize = 1_000_000;

/// A graph algorithm a `GRAPH_TABLE (graph CALL procedure(arguments))` runs (D1008).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlgorithmCall {
    pub(crate) procedure: &'static str,
    pub(crate) arguments: Vec<eggdb_graph::Value>,
}

fn argument_value(tokens: &[Token<'_>], at: &mut usize) -> SqlResult<eggdb_graph::Value> {
    let negative = matches!(tokens.get(*at).map(|token| &token.kind), Some(TokenKind::Operator("-")));
    if negative {
        *at += 1;
    }
    let token = tokens.get(*at).ok_or_else(|| invalid("a graph algorithm's arguments are not closed"))?;
    *at += 1;
    let sign = if negative { -1.0 } else { 1.0 };
    match &token.kind {
        TokenKind::Integer(digits) => digits
            .parse::<i64>()
            .map(|number| eggdb_graph::Value::Integer(if negative { -number } else { number }))
            .map_err(|_| invalid(format!("the argument {digits} is out of range"))),
        TokenKind::Decimal(digits) => digits
            .parse::<f64>()
            .map(|number| eggdb_graph::Value::Float(sign * number))
            .map_err(|_| invalid(format!("the argument {digits} is not a number"))),
        TokenKind::String(text) if !negative => Ok(eggdb_graph::Value::String(text.to_string())),
        TokenKind::Word(word) if !negative && word.eq_ignore_ascii_case("true") => Ok(eggdb_graph::Value::Boolean(true)),
        TokenKind::Word(word) if !negative && word.eq_ignore_ascii_case("false") => Ok(eggdb_graph::Value::Boolean(false)),
        TokenKind::Word(word) if !negative && word.eq_ignore_ascii_case("null") => Ok(eggdb_graph::Value::Null),
        _ => Err(invalid("a graph algorithm's arguments are numbers, strings, true, false or null")),
    }
}

/// The algorithm a `GRAPH_TABLE` body (`graph CALL procedure(arguments)`) runs, when it is one.
pub(crate) fn algorithm_call(body: &str) -> SqlResult<Option<AlgorithmCall>> {
    let tokens = tokenize_in(body, Dialect::PostgreSql)?;
    let mut reader = Reader { tokens: &tokens, at: 0 };
    if !reader.take_word("call") {
        return Ok(None);
    }
    let name = reader.name()?;
    let wanted = name.strip_prefix("eggdb.").unwrap_or(&name);
    let procedure = eggdb_graph::PROCEDURES
        .iter()
        .copied()
        .find(|procedure| procedure.strip_prefix("eggdb.").is_some_and(|short| short.eq_ignore_ascii_case(wanted)))
        .ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("there is no graph algorithm {name}; EggDB has {}", eggdb_graph::PROCEDURES.join(", ")),
            )
        })?;
    let mut arguments = Vec::new();
    if reader.take(&TokenKind::LeftParen) && !reader.take(&TokenKind::RightParen) {
        loop {
            arguments.push(argument_value(&tokens, &mut reader.at)?);
            if reader.take(&TokenKind::RightParen) {
                break;
            }
            reader.expect(&TokenKind::Comma, ", or )")?;
        }
    }
    if !matches!(tokens.get(reader.at).map(|token| &token.kind), None | Some(TokenKind::End)) {
        return Err(invalid("GRAPH_TABLE ... CALL takes one procedure call"));
    }
    Ok(Some(AlgorithmCall { procedure, arguments }))
}

/// The SQL that reads one element table for an algorithm: its name, and a query returning one
/// row of text values per vertex (its identity) or per edge (its source's identity, then its
/// destination's).
pub(crate) struct ElementQuery {
    pub(crate) vertex_from: String,
    pub(crate) vertex_to: String,
    pub(crate) source_columns: usize,
    pub(crate) query: String,
}

fn as_text(alias: &str, columns: &[String]) -> String {
    columns.iter().map(|column| format!("CAST({alias}.{column} AS text)")).collect::<Vec<_>>().join(", ")
}

impl PropertyGraph {
    fn identity(&self, vertex: &ElementTable) -> SqlResult<Vec<String>> {
        if !vertex.key.is_empty() {
            return Ok(vertex.key.clone());
        }
        self.edges
            .iter()
            .flat_map(|edge| [&edge.source, &edge.destination])
            .flatten()
            .find(|reference| self.vertex(&reference.vertex).is_some_and(|found| found.alias == vertex.alias))
            .map(|reference| reference.vertex_columns.clone())
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("the vertex table {} needs a KEY to run graph algorithms", vertex.alias),
                )
            })
    }

    /// The queries that read the graph's vertices and edges for an algorithm.
    pub(crate) fn algorithm_queries(&self) -> SqlResult<(Vec<ElementQuery>, Vec<ElementQuery>)> {
        let limit = MOST_ALGORITHM_ROWS + 1;
        let mut vertices = Vec::new();
        for vertex in &self.vertices {
            let identity = self.identity(vertex)?;
            vertices.push(ElementQuery {
                vertex_from: vertex.alias.clone(),
                vertex_to: vertex.alias.clone(),
                source_columns: identity.len(),
                query: format!("SELECT {} FROM {} AS v LIMIT {limit}", as_text("v", &identity), vertex.table),
            });
        }
        let mut edges = Vec::new();
        for edge in &self.edges {
            let (Some(source), Some(destination)) = (&edge.source, &edge.destination) else {
                continue;
            };
            let mut ends = Vec::new();
            for reference in [source, destination] {
                let vertex = self.vertex(&reference.vertex).ok_or_else(|| invalid(format!("no vertex table {}", reference.vertex)))?;
                if self.identity(vertex)? != reference.vertex_columns {
                    return Err(SqlError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("the edge table {} references columns of {} other than its key", edge.alias, vertex.alias),
                    ));
                }
                ends.push(vertex.alias.clone());
            }
            edges.push(ElementQuery {
                vertex_from: ends[0].clone(),
                vertex_to: ends[1].clone(),
                source_columns: source.columns.len(),
                query: format!(
                    "SELECT {}, {} FROM {} AS e LIMIT {limit}",
                    as_text("e", &source.columns),
                    as_text("e", &destination.columns),
                    edge.table
                ),
            });
        }
        Ok((vertices, edges))
    }
}

/// A vertex's name in an algorithm's output: its vertex table's alias and its key values.
pub(crate) fn vertex_name(alias: &str, key: &[String]) -> String {
    format!("{alias}:{}", key.join(","))
}

pub(crate) fn too_many_rows(rows: usize) -> SqlResult<()> {
    if rows > MOST_ALGORITHM_ROWS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("graph algorithms over tables read at most {MOST_ALGORITHM_ROWS} rows of each vertex and edge table"),
        ));
    }
    Ok(())
}

fn output_column(name: &str) -> (&'static str, &'static str) {
    match name {
        "nodeId" => ("node", "text"),
        "node1" => ("node1", "text"),
        "node2" => ("node2", "text"),
        "nodeIds" => ("nodes", "text"),
        "componentId" => ("component", "text"),
        "communityId" => ("community", "text"),
        "coreNumber" => ("core_number", "bigint"),
        "triangles" => ("triangles", "bigint"),
        "hops" => ("hops", "bigint"),
        "embedding" => ("embedding", "text"),
        "similarity" => ("similarity", "double precision"),
        "totalCost" => ("total_cost", "double precision"),
        _ => ("score", "double precision"),
    }
}

fn json_text(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            character if u32::from(character) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(character))),
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn output_value(column: &str, value: &eggdb_graph::Value, names: &[String]) -> String {
    let node = |value: &eggdb_graph::Value| match value {
        eggdb_graph::Value::Integer(id) => usize::try_from(*id).ok().and_then(|id| names.get(id)).cloned(),
        _ => None,
    };
    let text = |text: &str| crate::sqlserver_security::literal(text);
    match (output_column(column).1, value) {
        (_, eggdb_graph::Value::Null) => "NULL".to_owned(),
        ("text", eggdb_graph::Value::List(items)) if column == "nodeIds" => text(&format!(
            "[{}]",
            items.iter().filter_map(node).map(|name| json_text(&name)).collect::<Vec<_>>().join(",")
        )),
        ("text", eggdb_graph::Value::List(items)) => text(&format!(
            "[{}]",
            items
                .iter()
                .map(|item| match item {
                    eggdb_graph::Value::Float(number) if number.is_finite() => number.to_string(),
                    eggdb_graph::Value::Integer(number) => number.to_string(),
                    _ => "null".to_owned(),
                })
                .collect::<Vec<_>>()
                .join(",")
        )),
        ("text", value) => node(value).map_or_else(|| "NULL".to_owned(), |name| text(&name)),
        ("bigint", eggdb_graph::Value::Integer(number)) => format!("CAST({number} AS bigint)"),
        ("double precision", eggdb_graph::Value::Float(number)) => format!("CAST('{number}' AS double precision)"),
        ("double precision", eggdb_graph::Value::Integer(number)) => format!("CAST({number} AS double precision)"),
        _ => "NULL".to_owned(),
    }
}

/// The relation an algorithm's records stand for: its columns renamed for SQL, and node ids
/// written back as vertex names (`alias:key`).
pub(crate) fn algorithm_relation(columns: &[&str], records: &[Vec<eggdb_graph::Value>], names: &[String]) -> String {
    let named: Vec<(&str, &str)> = columns.iter().map(|column| output_column(column)).collect();
    if records.is_empty() {
        let empty = named.iter().map(|(name, kind)| format!("CAST(NULL AS {kind}) AS {name}")).collect::<Vec<_>>().join(", ");
        return format!("(SELECT {empty} WHERE 1 = 0)");
    }
    let rows = records
        .iter()
        .map(|record| {
            let values = columns
                .iter()
                .zip(record)
                .map(|(column, value)| output_value(column, value, names))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({values})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let names = named.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ");
    format!("(SELECT * FROM (VALUES {rows}) AS eggdb_graph_algorithm ({names}))")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_becomes_joins_of_the_tables_its_labels_name() {
        let (graph, _) = parse_definition(
            "CREATE PROPERTY GRAPH social VERTEX TABLES (person KEY (id) LABEL person PROPERTIES (id, full_name AS name)) \
             EDGE TABLES (knows KEY (a, b) SOURCE KEY (a) REFERENCES person (id) DESTINATION KEY (b) REFERENCES person (id) LABEL knows)",
        )
        .unwrap();
        let query = graph_table_query(
            &graph,
            " MATCH (x IS person)-[k IS knows]->(y IS person) WHERE x.name = 'Ada' COLUMNS (x.name AS from_name, y.name AS to_name)",
        )
        .unwrap();
        assert_eq!(
            query,
            "(SELECT x.full_name AS from_name, y.full_name AS to_name FROM person AS x JOIN knows AS k ON k.a = x.id JOIN person AS y ON k.b = y.id WHERE (x.full_name = 'Ada'))"
        );
        let either = graph_table_query(&graph, " MATCH (x)-[k]-(y) COLUMNS (x.id)").unwrap();
        assert_eq!(either.matches("UNION ALL").count(), 1);
        let two = graph_table_query(&graph, " MATCH (x)-[k]->(y), (y)-[j]->(z) COLUMNS (z.id)").unwrap();
        assert_eq!(
            two,
            "(SELECT z.id FROM person AS x JOIN knows AS k ON k.a = x.id JOIN person AS y ON k.b = y.id JOIN knows AS j ON j.a = y.id JOIN person AS z ON j.b = z.id)"
        );
        let cycle = graph_table_query(&graph, " MATCH (x)-[k]->(y)-[j]->(x) COLUMNS (x.id)").unwrap();
        assert!(cycle.ends_with("JOIN knows AS j ON j.a = y.id AND j.b = x.id)"));
        let quantified = graph_table_query(&graph, " MATCH (x)-[k]->{1,3}(y) COLUMNS (y.id)").unwrap();
        assert_eq!(quantified.matches("UNION ALL").count(), 2);
        assert!(quantified.contains("JOIN knows AS k_3 ON k_3.a = eggdb_node_2.id"));
        assert!(graph_table_query(&graph, " MATCH (x)-[k]->+(y) COLUMNS (y.id)").is_err());
        let spans = graph_tables("SELECT * FROM GRAPH_TABLE (social MATCH (a) COLUMNS (a.id)) AS g").unwrap();
        assert_eq!(spans[0].1, "social");
        assert_eq!(graph_command("create property graph g vertex tables (t)"), Some(true));
    }

    #[test]
    fn an_algorithm_call_names_a_procedure_and_its_records_become_a_relation() {
        let call = algorithm_call(" CALL pagerank(20, 0.85)").unwrap().unwrap();
        assert_eq!(call.procedure, "eggdb.pageRank");
        assert_eq!(call.arguments, vec![eggdb_graph::Value::Integer(20), eggdb_graph::Value::Float(0.85)]);
        assert!(algorithm_call(" MATCH (a) COLUMNS (a.id)").unwrap().is_none());
        assert!(algorithm_call(" CALL nothing()").is_err());
        let names = vec![vertex_name("person", &["1".to_owned()]), vertex_name("person", &["2".to_owned()])];
        let relation = algorithm_relation(
            &["nodeId", "score"],
            &[vec![eggdb_graph::Value::Integer(1), eggdb_graph::Value::Float(0.5)]],
            &names,
        );
        assert_eq!(
            relation,
            "(SELECT * FROM (VALUES ('person:2', CAST('0.5' AS double precision))) AS eggdb_graph_algorithm (node, score))"
        );
        assert_eq!(
            algorithm_relation(&["nodeId", "triangles"], &[], &names),
            "(SELECT CAST(NULL AS text) AS node, CAST(NULL AS bigint) AS triangles WHERE 1 = 0)"
        );
    }
}
