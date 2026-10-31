const MAX_CONDITIONS: usize = 200;

pub(crate) fn identifier(name: &str) -> Option<String> {
    let mut characters = name.chars();
    let first = characters.next()?;
    let valid = (first.is_alphabetic() || first == '_') && characters.all(|character| character.is_alphanumeric() || character == '_' || character == '$') && name.len() <= 63;
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
            b'%' => match text.get(at + 1..at + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok()) {
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
    let inner = text.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')).ok_or("in. needs a list in parentheses, such as in.(1,2,3)")?;
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
    let (operator, value) = expression.split_once('.').ok_or_else(|| format!("{expression}: write operator.value, such as eq.5"))?;
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
            _ => return Err(format!("is.{value}: use is.null, is.true, is.false or is.unknown")),
        },
        "in" => {
            let items = list_items(value)?;
            if items.is_empty() {
                "FALSE".to_owned()
            } else {
                format!("{column} IN ({})", items.iter().map(|item| literal(item)).collect::<Vec<_>>().join(", "))
            }
        }
        other => return Err(format!("{other} is not a filter operator this API knows")),
    };
    Ok(if negated { format!("NOT ({rendered})") } else { rendered })
}

#[derive(Debug, Default)]
pub(crate) struct Query {
    select: Option<Vec<String>>,
    conditions: Vec<String>,
    order: Vec<String>,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: Option<u64>,
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
                        for item in value.split(',').map(str::trim).filter(|item| !item.is_empty()) {
                            let (alias, column) = item.split_once(':').unwrap_or(("", item));
                            let column = identifier(column).ok_or_else(|| format!("{column} is not a column name"))?;
                            columns.push(if alias.is_empty() { column } else { format!("{column} AS {}", identifier(alias).ok_or_else(|| format!("{alias} is not a valid alias"))?) });
                        }
                        query.select = Some(columns);
                    }
                }
                "order" => {
                    for item in value.split(',').map(str::trim).filter(|item| !item.is_empty()) {
                        let mut parts = item.split('.');
                        let column = identifier(parts.next().unwrap_or_default()).ok_or_else(|| format!("{item} does not start with a column name"))?;
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
                "limit" => query.limit = Some(value.parse().map_err(|_| format!("limit={value} is not a count"))?),
                "offset" => query.offset = Some(value.parse().map_err(|_| format!("offset={value} is not a count"))?),
                column => {
                    let quoted = identifier(column).ok_or_else(|| format!("{column} is not a column name"))?;
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
        self.select.as_ref().map_or_else(|| "*".to_owned(), |columns| columns.join(", "))
    }

    pub(crate) fn where_clause(&self) -> String {
        if self.conditions.is_empty() { String::new() } else { format!(" WHERE {}", self.conditions.join(" AND ")) }
    }

    pub(crate) fn order_clause(&self) -> String {
        if self.order.is_empty() { String::new() } else { format!(" ORDER BY {}", self.order.join(", ")) }
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
