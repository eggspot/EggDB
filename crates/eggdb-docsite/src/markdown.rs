//! The Markdown EggDB's documentation is written in, rendered to HTML: ATX headings, paragraphs,
//! fenced code, bullet and numbered lists with nesting, tables, block quotes, rules, and inline
//! code, links, autolinks, strong and emphasis.

pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            out.push(character);
            dash = false;
        } else if (character == ' ' || character == '-') && !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

fn link_target(url: &str) -> String {
    let external = url.contains("://") || url.starts_with("mailto:") || url.starts_with('#');
    if external {
        return url.to_owned();
    }
    let (path, anchor) = match url.split_once('#') {
        Some((path, anchor)) => (path, Some(anchor)),
        None => (url, None),
    };
    let path = match path.strip_suffix(".md") {
        Some(stem) => format!("{stem}.html"),
        None => path.to_owned(),
    };
    match anchor {
        Some(anchor) => format!("{path}#{anchor}"),
        None => path,
    }
}

fn closing(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    let mut at = from;
    while at + marker.len() <= chars.len() {
        if chars[at..at + marker.len()] == *marker {
            return Some(at);
        }
        at += 1;
    }
    None
}

pub(crate) fn inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    while at < chars.len() {
        let rest = &chars[at..];
        if rest[0] == '`' {
            let ticks = rest.iter().take_while(|character| **character == '`').count();
            let fence = vec!['`'; ticks];
            if let Some(end) = closing(&chars, at + ticks, &fence) {
                let code: String = chars[at + ticks..end].iter().collect();
                out.push_str("<code>");
                out.push_str(&escape(code.trim()));
                out.push_str("</code>");
                at = end + ticks;
                continue;
            }
        }
        if rest[0] == '['
            && let Some(close) = closing(&chars, at + 1, &[']', '('])
            && let Some(end) = closing(&chars, close + 2, &[')'])
        {
            let label: String = chars[at + 1..close].iter().collect();
            let url: String = chars[close + 2..end].iter().collect();
            out.push_str(&format!("<a href=\"{}\">{}</a>", escape(&link_target(url.trim())), inline(&label)));
            at = end + 1;
            continue;
        }
        if rest[0] == '<'
            && let Some(end) = closing(&chars, at + 1, &['>'])
        {
            let url: String = chars[at + 1..end].iter().collect();
            if url.starts_with("http://") || url.starts_with("https://") {
                out.push_str(&format!("<a href=\"{0}\">{0}</a>", escape(&url)));
                at = end + 1;
                continue;
            }
        }
        if rest.starts_with(&['*', '*'])
            && let Some(end) = closing(&chars, at + 2, &['*', '*'])
            && end > at + 2
        {
            let inner: String = chars[at + 2..end].iter().collect();
            out.push_str(&format!("<strong>{}</strong>", inline(&inner)));
            at = end + 2;
            continue;
        }
        if rest[0] == '*'
            && rest.get(1).is_some_and(|next| !next.is_whitespace())
            && let Some(end) = closing(&chars, at + 1, &['*'])
            && end > at + 1
        {
            let inner: String = chars[at + 1..end].iter().collect();
            out.push_str(&format!("<em>{}</em>", inline(&inner)));
            at = end + 1;
            continue;
        }
        out.push_str(&escape(&rest[0].to_string()));
        at += 1;
    }
    out
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|character| *character == ' ').count()
}

fn list_marker(line: &str) -> Option<(bool, usize)> {
    let indent = indent_of(line);
    let trimmed = &line[indent..];
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return Some((false, indent + 2));
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        return Some((true, indent + digits + 2));
    }
    None
}

fn table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_code = false;
    let mut chars = inner.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '`' => {
                in_code = !in_code;
                cell.push(character);
            }
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cell).trim().to_owned()),
            other => cell.push(other),
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|')
        && trimmed.contains('-')
        && trimmed.chars().all(|character| matches!(character, '|' | '-' | ':' | ' '))
}

fn starts_block(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('#')
        || trimmed.starts_with("```")
        || trimmed.starts_with('>')
        || trimmed.starts_with('|')
        || list_marker(line).is_some()
        || trimmed == "---"
}

/// Renders a whole Markdown document as HTML, and its headings as `(level, text, id)` in order.
pub(crate) fn render(markdown: &str) -> (String, Vec<(usize, String, String)>) {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut headings = Vec::new();
    let html = blocks(&lines, &mut headings);
    (html, headings)
}

fn blocks(lines: &[&str], headings: &mut Vec<(usize, String, String)>) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            at += 1;
            continue;
        }
        if let Some(fence) = trimmed.strip_prefix("```") {
            let language = fence.trim();
            let mut code = Vec::new();
            at += 1;
            while at < lines.len() && !lines[at].trim_start().starts_with("```") {
                code.push(lines[at]);
                at += 1;
            }
            at += 1;
            let class = if language.is_empty() {
                String::new()
            } else {
                format!(" class=\"language-{}\"", escape(language))
            };
            out.push_str(&format!("<pre><code{class}>{}</code></pre>\n", escape(&code.join("\n"))));
            continue;
        }
        let hashes = trimmed.chars().take_while(|character| *character == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let text = trimmed[hashes..].trim();
            let id = slug(text);
            out.push_str(&format!("<h{hashes} id=\"{id}\">{}</h{hashes}>\n", inline(text)));
            headings.push((hashes, text.to_owned(), id));
            at += 1;
            continue;
        }
        if trimmed == "---" {
            out.push_str("<hr>\n");
            at += 1;
            continue;
        }
        if trimmed.starts_with('|') && lines.get(at + 1).is_some_and(|next| is_table_separator(next)) {
            out.push_str("<table>\n<thead><tr>");
            for cell in table_cells(trimmed) {
                out.push_str(&format!("<th>{}</th>", inline(&cell)));
            }
            out.push_str("</tr></thead>\n<tbody>\n");
            at += 2;
            while at < lines.len() && lines[at].trim().starts_with('|') {
                out.push_str("<tr>");
                for cell in table_cells(lines[at]) {
                    out.push_str(&format!("<td>{}</td>", inline(&cell)));
                }
                out.push_str("</tr>\n");
                at += 1;
            }
            out.push_str("</tbody>\n</table>\n");
            continue;
        }
        if trimmed.starts_with('>') {
            let mut quoted = Vec::new();
            while at < lines.len() && lines[at].trim_start().starts_with('>') {
                let inner = lines[at].trim_start()[1..].strip_prefix(' ').unwrap_or(&lines[at].trim_start()[1..]);
                quoted.push(inner);
                at += 1;
            }
            out.push_str(&format!("<blockquote>\n{}</blockquote>\n", blocks(&quoted, headings)));
            continue;
        }
        if let Some((ordered, _)) = list_marker(line) {
            at = list(lines, at, ordered, headings, &mut out);
            continue;
        }
        let mut paragraph = vec![trimmed];
        at += 1;
        while at < lines.len() && !lines[at].trim().is_empty() && !starts_block(lines[at]) {
            paragraph.push(lines[at].trim());
            at += 1;
        }
        out.push_str(&format!("<p>{}</p>\n", inline(&paragraph.join(" "))));
    }
    out
}

fn list(lines: &[&str], start: usize, ordered: bool, headings: &mut Vec<(usize, String, String)>, out: &mut String) -> usize {
    let base = indent_of(lines[start]);
    out.push_str(if ordered { "<ol>\n" } else { "<ul>\n" });
    let mut at = start;
    while at < lines.len() {
        let Some((item_ordered, content_at)) = list_marker(lines[at]) else {
            break;
        };
        if indent_of(lines[at]) != base || item_ordered != ordered {
            break;
        }
        let mut item = vec![&lines[at][content_at.min(lines[at].len())..]];
        at += 1;
        while at < lines.len() {
            let line = lines[at];
            if line.trim().is_empty() {
                let continues = lines.get(at + 1).is_some_and(|next| indent_of(next) > base && !next.trim().is_empty());
                if !continues {
                    break;
                }
                item.push("");
                at += 1;
                continue;
            }
            if indent_of(line) <= base {
                break;
            }
            let strip = (base + 2).min(indent_of(line));
            item.push(&line[strip..]);
            at += 1;
        }
        let body = blocks(&item, headings);
        out.push_str(&format!("<li>{}</li>\n", tight_item(&body)));
        while at < lines.len() && lines[at].trim().is_empty() {
            let next_item = lines
                .get(at + 1)
                .is_some_and(|next| indent_of(next) == base && list_marker(next).is_some());
            if !next_item {
                break;
            }
            at += 1;
        }
    }
    out.push_str(if ordered { "</ol>\n" } else { "</ul>\n" });
    at
}

fn tight_item(body: &str) -> String {
    let Some(rest) = body.strip_prefix("<p>") else {
        return body.to_owned();
    };
    let Some(end) = rest.find("</p>\n") else {
        return body.to_owned();
    };
    let (first, after) = (&rest[..end], &rest[end + "</p>\n".len()..]);
    if after.contains("<p>") {
        return body.to_owned();
    }
    if after.is_empty() {
        first.to_owned()
    } else {
        format!("{first}\n{after}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_paragraphs_and_inline_marks_render() {
        let (html, headings) = render("# EggDB *fast*\n\nOne `a < b` and **bold** with [a page](cli.md#serve).\nSame paragraph.");
        assert_eq!(
            html,
            "<h1 id=\"eggdb-fast\">EggDB <em>fast</em></h1>\n<p>One <code>a &lt; b</code> and <strong>bold</strong> with <a href=\"cli.html#serve\">a page</a>. Same paragraph.</p>\n"
        );
        assert_eq!(headings, vec![(1, "EggDB *fast*".to_owned(), "eggdb-fast".to_owned())]);
    }

    #[test]
    fn fenced_code_keeps_its_text_escaped() {
        let (html, _) = render("```sh\neggdb sql a.db -c \"SELECT 1 < 2\"\n```");
        assert_eq!(
            html,
            "<pre><code class=\"language-sh\">eggdb sql a.db -c &quot;SELECT 1 &lt; 2&quot;</code></pre>\n"
        );
    }

    #[test]
    fn nested_lists_and_continuation_lines_stay_in_their_items() {
        let (html, _) = render("- one\n  still one\n  - inner\n- two\n\n1. first\n2. second");
        assert_eq!(
            html,
            "<ul>\n<li>one still one\n<ul>\n<li>inner</li>\n</ul>\n</li>\n<li>two</li>\n</ul>\n<ol>\n<li>first</li>\n<li>second</li>\n</ol>\n"
        );
    }

    #[test]
    fn tables_split_cells_outside_code_and_keep_escaped_bars() {
        let (html, _) = render("| a | b |\n|---|---|\n| `x \\| y` | [l](https://e.org) |");
        assert_eq!(
            html,
            "<table>\n<thead><tr><th>a</th><th>b</th></tr></thead>\n<tbody>\n<tr><td><code>x | y</code></td><td><a href=\"https://e.org\">l</a></td></tr>\n</tbody>\n</table>\n"
        );
    }

    #[test]
    fn quotes_rules_and_autolinks_render() {
        let (html, _) = render("> quoted\n\n---\n\n<https://eggdb.dev>");
        assert_eq!(
            html,
            "<blockquote>\n<p>quoted</p>\n</blockquote>\n<hr>\n<p><a href=\"https://eggdb.dev\">https://eggdb.dev</a></p>\n"
        );
    }
}
