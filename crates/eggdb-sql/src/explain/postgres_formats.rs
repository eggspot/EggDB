const ARROW: &str = "->  ";

#[derive(Debug, Default)]
struct Node {
    node_type: String,
    relationship: Option<String>,
    subplan_name: Option<String>,
    details: Vec<(String, String)>,
    children: Vec<usize>,
}

fn describe(content: &str) -> (String, Vec<(String, String)>) {
    let mut details = Vec::new();
    let (head, relation) = match content.split_once(" on ") {
        Some((head, relation)) => (head, Some(relation)),
        None => (content, None),
    };
    let node_type = match head.split_once(" using ") {
        Some((node_type, index)) => {
            details.push(("Index Name".to_owned(), index.trim().to_owned()));
            node_type.trim().to_owned()
        }
        None => head.trim().to_owned(),
    };
    if let Some(relation) = relation {
        let mut parts = relation.split_whitespace();
        if let Some(name) = parts.next() {
            details.push(("Relation Name".to_owned(), name.to_owned()));
            details.push(("Alias".to_owned(), parts.next().unwrap_or(name).to_owned()));
        }
    }
    (node_type, details)
}

fn tree(lines: &[String]) -> (Vec<Node>, Option<usize>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<(usize, usize, Option<String>)> = Vec::new();
    let mut root = None;
    for line in lines {
        let indent = line.len() - line.trim_start().len();
        let content = line.trim_start();
        if content.is_empty() {
            continue;
        }
        if let Some(rest) = content.strip_prefix(ARROW).or_else(|| (root.is_none()).then_some(content)) {
            let arrow = if root.is_none() { 0 } else { indent };
            while stack.last().is_some_and(|(column, _, _)| *column >= arrow && root.is_some()) {
                stack.pop();
            }
            let (node_type, details) = describe(rest);
            let index = nodes.len();
            let mut node = Node {
                node_type,
                details,
                ..Node::default()
            };
            match stack.last_mut() {
                Some((_, parent, label)) => {
                    let position = nodes[*parent].children.len();
                    match label.take() {
                        Some(label) => {
                            let kind = label.split_whitespace().next().unwrap_or("SubPlan").to_owned();
                            node.relationship = Some(kind);
                            node.subplan_name = Some(label);
                        }
                        None => {
                            node.relationship = Some(match position {
                                0 => "Outer",
                                1 => "Inner",
                                _ => "Member",
                            }
                            .to_owned());
                        }
                    }
                    let parent = *parent;
                    nodes.push(node);
                    nodes[parent].children.push(index);
                }
                None => {
                    nodes.push(node);
                    root = Some(index);
                }
            }
            let column = if indent == 0 && stack.is_empty() { 0 } else { indent + ARROW.len() };
            stack.push((column, index, None));
            continue;
        }
        while stack.len() > 1 && stack.last().is_some_and(|(column, _, _)| *column >= indent) {
            stack.pop();
        }
        let Some((_, owner, label)) = stack.last_mut() else {
            continue;
        };
        if content.starts_with("SubPlan ") || content.starts_with("InitPlan ") {
            *label = Some(content.split(" (").next().unwrap_or(content).to_owned());
        } else if let Some((key, value)) = content.split_once(": ") {
            nodes[*owner].details.push((key.to_owned(), value.to_owned()));
        } else {
            nodes[*owner].details.push(("Detail".to_owned(), content.to_owned()));
        }
    }
    (nodes, root)
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn node_json(nodes: &[Node], index: usize, indent: usize, out: &mut String) {
    let node = &nodes[index];
    let pad = " ".repeat(indent);
    let mut fields = vec![format!("{pad}  \"Node Type\": {}", json_string(&node.node_type))];
    if let Some(relationship) = &node.relationship {
        fields.push(format!("{pad}  \"Parent Relationship\": {}", json_string(relationship)));
    }
    if let Some(name) = &node.subplan_name {
        fields.push(format!("{pad}  \"Subplan Name\": {}", json_string(name)));
    }
    for (key, value) in &node.details {
        fields.push(format!("{pad}  {}: {}", json_string(key), json_string(value)));
    }
    if !node.children.is_empty() {
        let mut children = String::new();
        for (at, child) in node.children.iter().enumerate() {
            if at > 0 {
                children.push_str(",\n");
            }
            node_json(nodes, *child, indent + 4, &mut children);
        }
        fields.push(format!("{pad}  \"Plans\": [\n{children}\n{pad}  ]"));
    }
    out.push_str(&format!("{pad}{{\n{}\n{pad}}}", fields.join(",\n")));
}

fn yaml_value(text: &str) -> String {
    json_string(text)
}

fn node_yaml(nodes: &[Node], index: usize, indent: usize, out: &mut String) {
    let node = &nodes[index];
    let pad = " ".repeat(indent);
    out.push_str(&format!("{pad}Node Type: {}\n", yaml_value(&node.node_type)));
    if let Some(relationship) = &node.relationship {
        out.push_str(&format!("{pad}Parent Relationship: {}\n", yaml_value(relationship)));
    }
    if let Some(name) = &node.subplan_name {
        out.push_str(&format!("{pad}Subplan Name: {}\n", yaml_value(name)));
    }
    for (key, value) in &node.details {
        out.push_str(&format!("{pad}{key}: {}\n", yaml_value(value)));
    }
    if !node.children.is_empty() {
        out.push_str(&format!("{pad}Plans: \n"));
        for child in &node.children {
            let mut inner = String::new();
            node_yaml(nodes, *child, indent + 4, &mut inner);
            let mut lines = inner.lines();
            if let Some(first) = lines.next() {
                out.push_str(&format!("{pad}  - {}\n", first.trim_start()));
            }
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
}

/// The PostgreSQL text plan `lines` as `EXPLAIN (FORMAT JSON)` writes a plan: one object with
/// its `Plan` tree, each node's type, relation, alias, index, conditions and `Plans`.
pub(crate) fn json(lines: &[String]) -> String {
    let (nodes, root) = tree(lines);
    let Some(root) = root else {
        return "[]".to_owned();
    };
    let mut plan = String::new();
    node_json(&nodes, root, 4, &mut plan);
    format!("[\n  {{\n    \"Plan\": {}\n  }}\n]", plan.trim_start())
}

/// The PostgreSQL text plan `lines` as `EXPLAIN (FORMAT YAML)` writes a plan.
pub(crate) fn yaml(lines: &[String]) -> String {
    let (nodes, root) = tree(lines);
    let Some(root) = root else {
        return String::new();
    };
    let mut plan = String::new();
    node_yaml(&nodes, root, 4, &mut plan);
    format!("- Plan: \n{}", plan.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn text_plans_become_trees() {
        let plan = lines(&[
            "Seq Scan on tenk1 t",
            "  Filter: (SubPlan 1)",
            "  SubPlan 1",
            "    ->  Seq Scan on onek o",
            "          Filter: (four = t.four)",
        ]);
        let (nodes, root) = tree(&plan);
        let root = root.unwrap();
        assert_eq!(nodes[root].node_type, "Seq Scan");
        assert_eq!(nodes[root].children.len(), 1);
        let child = &nodes[nodes[root].children[0]];
        assert_eq!(child.relationship.as_deref(), Some("SubPlan"));
        assert_eq!(child.subplan_name.as_deref(), Some("SubPlan 1"));
        assert!(child.details.contains(&("Filter".to_owned(), "(four = t.four)".to_owned())));
        let written = json(&plan);
        assert!(written.contains("\"Relation Name\": \"onek\""), "{written}");
        assert!(yaml(&plan).starts_with("- Plan: \n    Node Type: \"Seq Scan\""));
    }
}
