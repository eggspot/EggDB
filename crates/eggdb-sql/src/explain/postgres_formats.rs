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

const ACTUAL_GROUP: &str = " (actual time=";
const NEVER_EXECUTED: &str = " (never executed)";

fn measured(content: &str) -> (&str, Vec<(String, String)>) {
    if let Some(at) = content.find(NEVER_EXECUTED) {
        return (&content[..at], vec![("Actual Loops".to_owned(), "0".to_owned())]);
    }
    let Some(at) = content.find(ACTUAL_GROUP) else {
        return (content, Vec::new());
    };
    let group = content[at + ACTUAL_GROUP.len()..].trim_end_matches(')');
    let mut details = Vec::new();
    let mut words = group.split_whitespace();
    if let Some((startup, total)) = words.next().and_then(|times| times.split_once("..")) {
        details.push(("Actual Startup Time".to_owned(), startup.to_owned()));
        details.push(("Actual Total Time".to_owned(), total.to_owned()));
    }
    for word in words {
        match word.split_once('=') {
            Some(("rows", rows)) => details.push(("Actual Rows".to_owned(), rows.to_owned())),
            Some(("loops", loops)) => details.push(("Actual Loops".to_owned(), loops.to_owned())),
            _ => {}
        }
    }
    (&content[..at], details)
}

fn is_number_field(key: &str) -> bool {
    key.starts_with("Actual ")
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
            let (rest, actual) = measured(rest);
            let (node_type, mut details) = describe(rest);
            details.extend(actual);
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
        let shown = if is_number_field(key) && value.parse::<f64>().is_ok() {
            value.clone()
        } else {
            json_string(value)
        };
        fields.push(format!("{pad}  {}: {shown}", json_string(key)));
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
        let shown = if is_number_field(key) && value.parse::<f64>().is_ok() {
            value.clone()
        } else {
            yaml_value(value)
        };
        out.push_str(&format!("{pad}{key}: {shown}\n"));
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

fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_element(name: &str) -> String {
    name.replace(' ', "-")
}

fn node_xml(nodes: &[Node], index: usize, indent: usize, out: &mut String) {
    let node = &nodes[index];
    let pad = " ".repeat(indent);
    out.push_str(&format!("{pad}<Plan>\n"));
    let mut field = |name: &str, value: &str| {
        let element = xml_element(name);
        out.push_str(&format!("{pad}  <{element}>{}</{element}>\n", xml_text(value)));
    };
    field("Node Type", &node.node_type);
    if let Some(relationship) = &node.relationship {
        field("Parent Relationship", relationship);
    }
    if let Some(name) = &node.subplan_name {
        field("Subplan Name", name);
    }
    for (key, value) in &node.details {
        field(key, value);
    }
    if !node.children.is_empty() {
        out.push_str(&format!("{pad}  <Plans>\n"));
        for child in &node.children {
            node_xml(nodes, *child, indent + 4, out);
        }
        out.push_str(&format!("{pad}  </Plans>\n"));
    }
    out.push_str(&format!("{pad}</Plan>\n"));
}

/// The PostgreSQL text plan `lines` as `EXPLAIN (FORMAT XML)` writes a plan: an `explain`
/// document in PostgreSQL's namespace with one `Query` and its `Plan` tree, each field an
/// element named as the JSON key with hyphens for spaces.
pub(crate) fn xml(lines: &[String]) -> String {
    let (nodes, root) = tree(lines);
    let mut plan = String::new();
    if let Some(root) = root {
        node_xml(&nodes, root, 4, &mut plan);
    }
    format!(
        "<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n  <Query>\n{plan}  </Query>\n</explain>"
    )
}

/// An `EXPLAIN (ANALYZE, FORMAT JSON)` plan: [`json`]'s tree with each node's `Actual`
/// fields as numbers, and the planning and execution times beside the plan.
pub(crate) fn json_analyzed(lines: &[String], planning_ms: f64, execution_ms: f64) -> String {
    let plan = json(lines);
    match plan.rfind("\n  }\n]") {
        Some(at) => format!(
            "{},\n    \"Planning Time\": {planning_ms:.3},\n    \"Execution Time\": {execution_ms:.3}{}",
            &plan[..at],
            &plan[at..]
        ),
        None => plan,
    }
}

/// An `EXPLAIN (ANALYZE, FORMAT YAML)` plan, with the planning and execution times.
pub(crate) fn yaml_analyzed(lines: &[String], planning_ms: f64, execution_ms: f64) -> String {
    format!(
        "{}\n  Planning Time: {planning_ms:.3}\n  Execution Time: {execution_ms:.3}",
        yaml(lines)
    )
}

/// An `EXPLAIN (ANALYZE, FORMAT XML)` plan, with the planning and execution times.
pub(crate) fn xml_analyzed(lines: &[String], planning_ms: f64, execution_ms: f64) -> String {
    xml(lines).replacen(
        "  </Query>",
        &format!("    <Planning-Time>{planning_ms:.3}</Planning-Time>\n    <Execution-Time>{execution_ms:.3}</Execution-Time>\n  </Query>"),
        1,
    )
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
        let document = xml(&plan);
        assert!(document.starts_with("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n  <Query>\n    <Plan>\n      <Node-Type>Seq Scan</Node-Type>"), "{document}");
        assert!(document.contains("<Relation-Name>onek</Relation-Name>"), "{document}");
        assert!(document.contains("<Filter>(four = t.four)</Filter>"), "{document}");
        assert!(document.ends_with("  </Query>\n</explain>"), "{document}");
    }

    #[test]
    fn measured_plans_carry_actual_fields_and_times() {
        let plan = lines(&[
            "Seq Scan on t (actual time=0.010..0.020 rows=3 loops=1)",
            "  Filter: (a > 1)",
        ]);
        let json = json_analyzed(&plan, 0.5, 1.25);
        assert!(json.contains("\"Relation Name\": \"t\""), "{json}");
        assert!(json.contains("\"Actual Total Time\": 0.020"), "{json}");
        assert!(json.contains("\"Actual Rows\": 3"), "{json}");
        assert!(json.contains("\"Execution Time\": 1.250"), "{json}");
        assert!(crate::json::parse_document(&json).is_ok(), "{json}");
        let never = json_analyzed(&lines(&["Seq Scan on t (never executed)"]), 0.0, 0.0);
        assert!(never.contains("\"Actual Loops\": 0"), "{never}");
        assert!(yaml_analyzed(&plan, 0.5, 1.25).ends_with("Execution Time: 1.250"));
        assert!(xml_analyzed(&plan, 0.5, 1.25).contains("<Planning-Time>0.500</Planning-Time>"));
    }
}
