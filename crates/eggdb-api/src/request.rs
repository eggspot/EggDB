const MAX_CONDITIONS: usize = 200;

pub(crate) fn identifier(name: &str) -> Option<String> {
    let mut characters = name.chars();
    let first = characters.next()?;
    let valid = (first.is_alphabetic() || first == '_')
        && characters
            .all(|character| character.is_alphanumeric() || character == '_' || character == '$')
        && name.len() <= 63;
    valid.then(|| format!("\"{name}\""))
}

pub(crate) fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' => match text
                .get(at + 1..at + 3)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    at += 2;
                }
                None => out.push(b'%'),
            },
            other => out.push(other),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn list_items(text: &str) -> Result<Vec<String>, String> {
    let inner = text
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or("in. needs a list in parentheses, such as in.(1,2,3)")?;
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut characters = inner.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '"' if quoted && characters.peek() == Some(&'"') => {
                current.push('"');
                characters.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => items.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    if quoted {
        return Err("a quoted list item is not closed".to_owned());
    }
    if !inner.is_empty() {
        items.push(current);
    }
    Ok(items)
}

fn condition(column: &str, expression: &str) -> Result<String, String> {
    let (negated, expression) = match expression.strip_prefix("not.") {
        Some(rest) => (true, rest),
        None => (false, expression),
    };
    let (operator, value) = expression
        .split_once('.')
        .ok_or_else(|| format!("{expression}: write operator.value, such as eq.5"))?;
    let rendered = match operator {
        "eq" => format!("{column} = {}", literal(value)),
        "neq" => format!("{column} <> {}", literal(value)),
        "gt" => format!("{column} > {}", literal(value)),
        "gte" => format!("{column} >= {}", literal(value)),
        "lt" => format!("{column} < {}", literal(value)),
        "lte" => format!("{column} <= {}", literal(value)),
        "like" => format!("{column} LIKE {}", literal(&value.replace('*', "%"))),
        "ilike" => format!("{column} ILIKE {}", literal(&value.replace('*', "%"))),
        "is" => match value.to_ascii_lowercase().as_str() {
            "null" => format!("{column} IS NULL"),
            "true" => format!("{column} IS TRUE"),
            "false" => format!("{column} IS FALSE"),
            "unknown" => format!("{column} IS UNKNOWN"),
            _ => {
                return Err(format!(
                    "is.{value}: use is.null, is.true, is.false or is.unknown"
                ));
            }
        },
        "in" => {
            let items = list_items(value)?;
            if items.is_empty() {
                "FALSE".to_owned()
            } else {
                format!(
                    "{column} IN ({})",
                    items
                        .iter()
                        .map(|item| literal(item))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        other => return Err(format!("{other} is not a filter operator this API knows")),
    };
    Ok(if negated {
        format!("NOT ({rendered})")
    } else {
        rendered
    })
}

const MAX_GROUP_DEPTH: usize = 16;

fn split_top_level(text: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            '(' if !quoted => {
                depth += 1;
                current.push(character);
            }
            ')' if !quoted => {
                depth = depth
                    .checked_sub(1)
                    .ok_or("a logic group closes a parenthesis it never opened")?;
                current.push(character);
            }
            ',' if !quoted && depth == 0 => items.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    if quoted || depth != 0 {
        return Err("a logic group is not closed".to_owned());
    }
    if !current.is_empty() {
        items.push(current);
    }
    Ok(items)
}

fn unquoted_value(expression: &str) -> String {
    match expression.split_once('.') {
        Some((operator, value))
            if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') =>
        {
            format!("{operator}.{}", value[1..value.len() - 1].replace("\"\"", "\""))
        }
        _ => expression.to_owned(),
    }
}

fn logic_item(item: &str, depth: usize) -> Result<String, String> {
    let item = item.trim();
    let (negated, rest) = match item.strip_prefix("not.") {
        Some(rest) if rest.starts_with("and(") || rest.starts_with("or(") => (true, rest),
        _ => (false, item),
    };
    for (word, joiner) in [("and", " AND "), ("or", " OR ")] {
        if let Some(inner) = rest
            .strip_prefix(word)
            .and_then(|rest| rest.strip_prefix('('))
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let group = logic_group(inner, joiner, depth + 1)?;
            return Ok(if negated {
                format!("NOT {group}")
            } else {
                group
            });
        }
    }
    let (column, expression) = item
        .split_once('.')
        .ok_or_else(|| format!("{item}: write column.operator.value inside a logic group"))?;
    let quoted =
        identifier(column).ok_or_else(|| format!("{column} is not a column name"))?;
    condition(&quoted, &unquoted_value(expression))
}

fn logic_group(text: &str, joiner: &str, depth: usize) -> Result<String, String> {
    if depth > MAX_GROUP_DEPTH {
        return Err("logic groups are nested too deeply".to_owned());
    }
    let items = split_top_level(text)?;
    if items.is_empty() {
        return Err("a logic group needs at least one condition".to_owned());
    }
    let rendered = items
        .iter()
        .map(|item| logic_item(item, depth))
        .collect::<Result<Vec<String>, String>>()?;
    Ok(format!("({})", rendered.join(joiner)))
}

#[derive(Debug, Default)]
pub(crate) struct Query {
    select: Option<Vec<String>>,
    conditions: Vec<String>,
    order: Vec<String>,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: Option<u64>,
    pub(crate) on_conflict: Option<Vec<String>>,
}

pub(crate) fn call_arguments(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut arguments = Vec::new();
    for pair in text.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (name, value) = (percent_decode(name), percent_decode(value));
        let parameter =
            identifier(&name).ok_or_else(|| format!("{name} is not a parameter name"))?;
        if arguments.len() >= MAX_CONDITIONS {
            return Err("too many arguments".to_owned());
        }
        arguments.push((parameter, value));
    }
    Ok(arguments)
}

impl Query {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut query = Self::default();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let (name, value) = (percent_decode(name), percent_decode(value));
            match name.as_str() {
                "select" => {
                    if value.trim() != "*" {
                        let mut columns = Vec::new();
                        for item in value
                            .split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                        {
                            let (alias, column) = item.split_once(':').unwrap_or(("", item));
                            let column = identifier(column)
                                .ok_or_else(|| format!("{column} is not a column name"))?;
                            columns.push(if alias.is_empty() {
                                column
                            } else {
                                format!(
                                    "{column} AS {}",
                                    identifier(alias)
                                        .ok_or_else(|| format!("{alias} is not a valid alias"))?
                                )
                            });
                        }
                        query.select = Some(columns);
                    }
                }
                "order" => {
                    for item in value
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                    {
                        let mut parts = item.split('.');
                        let column = identifier(parts.next().unwrap_or_default())
                            .ok_or_else(|| format!("{item} does not start with a column name"))?;
                        let mut rendered = column;
                        for modifier in parts {
                            rendered.push_str(match modifier {
                                "asc" => " ASC",
                                "desc" => " DESC",
                                "nullsfirst" => " NULLS FIRST",
                                "nullslast" => " NULLS LAST",
                                other => return Err(format!("{other} is not an order modifier")),
                            });
                        }
                        query.order.push(rendered);
                    }
                }
                "limit" => {
                    query.limit = Some(
                        value
                            .parse()
                            .map_err(|_| format!("limit={value} is not a count"))?,
                    )
                }
                "offset" => {
                    query.offset = Some(
                        value
                            .parse()
                            .map_err(|_| format!("offset={value} is not a count"))?,
                    )
                }
                "on_conflict" => {
                    let mut columns = Vec::new();
                    for column in value.split(',').map(str::trim).filter(|item| !item.is_empty()) {
                        columns.push(
                            identifier(column)
                                .ok_or_else(|| format!("{column} is not a column name"))?,
                        );
                    }
                    query.on_conflict = Some(columns);
                }
                "or" | "and" | "not.or" | "not.and" => {
                    let inner = value
                        .strip_prefix('(')
                        .and_then(|rest| rest.strip_suffix(')'))
                        .ok_or_else(|| format!("{name} needs its conditions in parentheses"))?;
                    let joiner = if name.ends_with("or") { " OR " } else { " AND " };
                    let group = logic_group(inner, joiner, 0)?;
                    if query.conditions.len() >= MAX_CONDITIONS {
                        return Err("too many filters".to_owned());
                    }
                    query.conditions.push(if name.starts_with("not.") {
                        format!("NOT {group}")
                    } else {
                        group
                    });
                }
                column => {
                    let quoted = identifier(column)
                        .ok_or_else(|| format!("{column} is not a column name"))?;
                    if query.conditions.len() >= MAX_CONDITIONS {
                        return Err("too many filters".to_owned());
                    }
                    query.conditions.push(condition(&quoted, &value)?);
                }
            }
        }
        Ok(query)
    }

    pub(crate) fn projection(&self) -> String {
        self.select
            .as_ref()
            .map_or_else(|| "*".to_owned(), |columns| columns.join(", "))
    }

    pub(crate) fn where_clause(&self) -> String {
        if self.conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.conditions.join(" AND "))
        }
    }

    pub(crate) fn order_clause(&self) -> String {
        if self.order.is_empty() {
            String::new()
        } else {
            format!(" ORDER BY {}", self.order.join(", "))
        }
    }

    pub(crate) fn page_clause(&self) -> String {
        let mut clause = String::new();
        if let Some(limit) = self.limit {
            clause.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = self.offset {
            clause.push_str(&format!(" OFFSET {offset}"));
        }
        clause
    }
}
