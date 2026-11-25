use crate::markdown::{escape, render};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const STYLE: &str = "body{margin:0;font:16px/1.6 system-ui,-apple-system,'Segoe UI',sans-serif;color:#1d1d1f;background:#fff}\
header{background:#14213d;padding:.6rem 1.5rem}header a{color:#fff;margin-right:1rem;text-decoration:none}\
header a.home{font-weight:700;color:#fca311}main{max-width:52rem;margin:0 auto;padding:1rem 1.5rem 4rem}\
nav.toc{border-left:3px solid #fca311;padding-left:1rem;margin:1rem 0}nav.toc a{display:block;color:#14213d}\
pre{background:#f4f4f6;padding:.8rem;overflow-x:auto;border-radius:6px}code{font:14px ui-monospace,Menlo,Consolas,monospace}\
table{border-collapse:collapse;margin:1rem 0}th,td{border:1px solid #d0d0d6;padding:.3rem .6rem;vertical-align:top}\
blockquote{border-left:3px solid #d0d0d6;margin:0;padding-left:1rem;color:#555}\
@media (prefers-color-scheme:dark){body{background:#111;color:#e8e8ea}pre{background:#1d1d22}nav.toc a{color:#fca311}th,td{border-color:#333}}";

struct Page {
    source: PathBuf,
    relative: PathBuf,
}

fn markdown_pages(root: &Path, directory: &Path, pages: &mut Vec<Page>, others: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            markdown_pages(root, &path, pages, others)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(io::Error::other)?.to_path_buf();
        if path.extension().is_some_and(|extension| extension == "md") {
            pages.push(Page { source: path, relative });
        } else {
            others.push(relative);
        }
    }
    Ok(())
}

fn html_path(relative: &Path) -> PathBuf {
    relative.with_extension("html")
}

fn up_to_root(relative: &Path) -> String {
    "../".repeat(relative.components().count().saturating_sub(1))
}

fn plain(text: &str) -> String {
    text.chars().filter(|character| !matches!(character, '*' | '`')).collect()
}

fn title_of(markdown: &str, relative: &Path) -> String {
    markdown
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(|title| plain(title.trim()))
        .unwrap_or_else(|| relative.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default())
}

fn navigation(top: &[(PathBuf, String)], prefix: &str) -> String {
    let mut out = format!("<header><a class=\"home\" href=\"{prefix}index.html\">EggDB</a>");
    for (path, title) in top {
        out.push_str(&format!(
            "<a href=\"{prefix}{}\">{}</a>",
            escape(&html_path(path).to_string_lossy()),
            escape(title)
        ));
    }
    out.push_str("</header>");
    out
}

fn table_of_contents(headings: &[(usize, String, String)]) -> String {
    let sections: Vec<_> = headings.iter().filter(|(level, _, _)| *level == 2).collect();
    if sections.len() < 3 {
        return String::new();
    }
    let mut out = String::from("<nav class=\"toc\">");
    for (_, text, id) in sections {
        out.push_str(&format!("<a href=\"#{id}\">{}</a>", escape(&plain(text))));
    }
    out.push_str("</nav>");
    out
}

fn document(title: &str, header: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{} · EggDB</title>\n<style>{STYLE}</style>\n</head>\n<body>\n{header}\n<main>\n{body}</main>\n</body>\n</html>\n",
        escape(title)
    )
}

fn write(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

/// Builds the site from `docs` into `output`; answers how many pages it wrote.
pub(crate) fn build(docs: &Path, output: &Path) -> io::Result<usize> {
    let mut pages = Vec::new();
    let mut others = Vec::new();
    markdown_pages(docs, docs, &mut pages, &mut others)?;
    let mut titled = Vec::with_capacity(pages.len());
    for page in &pages {
        let markdown = fs::read_to_string(&page.source)?;
        titled.push((title_of(&markdown, &page.relative), markdown));
    }
    let top: Vec<(PathBuf, String)> = pages
        .iter()
        .zip(&titled)
        .filter(|(page, _)| page.relative.components().count() == 1)
        .map(|(page, (title, _))| (page.relative.clone(), title.clone()))
        .collect();
    for (page, (title, markdown)) in pages.iter().zip(&titled) {
        let (body, headings) = render(markdown);
        let prefix = up_to_root(&page.relative);
        let contents = format!("{}{body}", table_of_contents(&headings));
        write(
            &output.join(html_path(&page.relative)),
            &document(title, &navigation(&top, &prefix), &contents),
        )?;
    }
    let mut index = String::from("<h1 id=\"eggdb-documentation\">EggDB documentation</h1>\n<ul>\n");
    for (page, (title, _)) in pages.iter().zip(&titled) {
        index.push_str(&format!(
            "<li><a href=\"{}\">{}</a></li>\n",
            escape(&html_path(&page.relative).to_string_lossy()),
            escape(title)
        ));
    }
    index.push_str("</ul>\n");
    write(&output.join("index.html"), &document("Documentation", &navigation(&top, ""), &index))?;
    for other in &others {
        let target = output.join(other);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(docs.join(other), target)?;
    }
    Ok(pages.len() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("eggdb-docsite-{}-{name}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn every_page_is_written_with_links_to_html_and_a_navigation_bar() {
        let docs = scratch("docs");
        fs::create_dir_all(docs.join("decisions")).unwrap();
        fs::write(docs.join("cli.md"), "# The CLI\n\nSee [D1](decisions/D001.md).").unwrap();
        fs::write(docs.join("decisions").join("D001.md"), "## D1 First\n\nBack to [the CLI](../cli.md).").unwrap();
        fs::write(docs.join("logo.png"), [1, 2, 3]).unwrap();
        let output = scratch("site");
        assert_eq!(build(&docs, &output).unwrap(), 3);
        let cli = fs::read_to_string(output.join("cli.html")).unwrap();
        assert!(cli.contains("<a href=\"decisions/D001.html\">D1</a>"), "{cli}");
        assert!(cli.contains("<title>The CLI · EggDB</title>"), "{cli}");
        let decision = fs::read_to_string(output.join("decisions").join("D001.html")).unwrap();
        assert!(decision.contains("href=\"../index.html\""), "{decision}");
        assert!(decision.contains("<a href=\"../cli.html\">the CLI</a>"), "{decision}");
        assert!(fs::read_to_string(output.join("index.html")).unwrap().contains("decisions/D001.html"));
        assert_eq!(fs::read(output.join("logo.png")).unwrap(), vec![1, 2, 3]);
    }
}
