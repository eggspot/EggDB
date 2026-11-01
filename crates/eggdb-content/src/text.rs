struct Style<'s> {
    paragraphs: &'s [&'s str],
    text_inside: &'s [&'s str],
    tabs: &'s [&'s str],
    breaks: &'s [&'s str],
    spaces: &'s [&'s str],
}

const WORD: Style<'static> = Style {
    paragraphs: &["w:p"],
    text_inside: &["w:t"],
    tabs: &["w:tab"],
    breaks: &["w:br", "w:cr"],
    spaces: &[],
};
const SHARED_STRINGS: Style<'static> = Style {
    paragraphs: &["si"],
    text_inside: &["t"],
    tabs: &[],
    breaks: &[],
    spaces: &[],
};
const SLIDES: Style<'static> = Style {
    paragraphs: &["a:p"],
    text_inside: &["a:t"],
    tabs: &[],
    breaks: &["a:br"],
    spaces: &[],
};
const OPEN_DOCUMENT: Style<'static> = Style {
    paragraphs: &["text:p", "text:h"],
    text_inside: &["text:p", "text:h"],
    tabs: &["text:tab"],
    breaks: &["text:line-break"],
    spaces: &["text:s"],
};

fn decode_entities(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after
            .char_indices()
            .take(11)
            .find(|(_, character)| *character == ';')
            .map(|(end, _)| end)
        else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .map(|hex| u32::from_str_radix(hex, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
}

fn xml_text(xml: &str, style: &Style<'_>) -> String {
    let mut out = String::new();
    let mut inside = 0usize;
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            if inside > 0 {
                decode_entities(rest, &mut out);
            }
            break;
        };
        if inside > 0 {
            decode_entities(&rest[..open], &mut out);
        }
        let tag_text = &rest[open..];
        if let Some(cdata) = tag_text.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            if inside > 0 {
                out.push_str(&cdata[..end]);
            }
            rest = cdata.get(end + 3..).unwrap_or_default();
            continue;
        }
        let terminator = if tag_text.starts_with("<!--") {
            "-->"
        } else {
            ">"
        };
        let Some(close) = tag_text.find(terminator) else {
            break;
        };
        let tag = &tag_text[1..close];
        rest = &tag_text[close + terminator.len()..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let closing = tag.starts_with('/');
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|character: char| character.is_whitespace() || character == '/')
            .next()
            .unwrap_or_default();
        if closing {
            if style.text_inside.contains(&name) {
                inside = inside.saturating_sub(1);
            }
            if style.paragraphs.contains(&name) && !out.ends_with('\n') {
                out.push('\n');
            }
            continue;
        }
        if style.tabs.contains(&name) {
            out.push('\t');
        } else if style.breaks.contains(&name) {
            out.push('\n');
        } else if style.spaces.contains(&name) {
            let count = tag
                .split_once("text:c=\"")
                .and_then(|(_, rest)| rest.split('"').next())
                .and_then(|count| count.parse::<usize>().ok())
                .unwrap_or(1)
                .min(64);
            out.extend(std::iter::repeat_n(' ', count));
        } else if !self_closing && style.text_inside.contains(&name) {
            inside += 1;
        } else if self_closing && style.paragraphs.contains(&name) {
            out.push('\n');
        }
    }
    out.trim_end().to_owned()
}

fn slide_number(name: &str) -> Option<u32> {
    name.strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

pub(crate) fn extract(
    archive: &crate::ZipArchive<'_>,
    limit: usize,
) -> Result<String, crate::ContentError> {
    let text = extract_within(archive, limit)?;
    if text.len() > limit {
        return Err(crate::ContentError(format!(
            "the document's text is longer than {limit} bytes"
        )));
    }
    Ok(text)
}

fn extract_within(
    archive: &crate::ZipArchive<'_>,
    limit: usize,
) -> Result<String, crate::ContentError> {
    let left = std::cell::Cell::new(limit);
    let read = |name: &str| -> Result<Option<String>, crate::ContentError> {
        let Some(bytes) = archive.read(name, left.get())? else {
            return Ok(None);
        };
        left.set(left.get().saturating_sub(bytes.len()));
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    };
    if let Some(document) = read("word/document.xml")? {
        return Ok(xml_text(&document, &WORD));
    }
    if let Some(strings) = read("xl/sharedStrings.xml")? {
        return Ok(xml_text(&strings, &SHARED_STRINGS));
    }
    let mut slides: Vec<(u32, &str)> = archive
        .entries
        .iter()
        .filter_map(|entry| slide_number(&entry.name).map(|number| (number, entry.name.as_str())))
        .collect();
    if !slides.is_empty() {
        slides.sort_unstable();
        let mut texts = Vec::new();
        for (_, name) in slides {
            texts.push(xml_text(&read(name)?.unwrap_or_default(), &SLIDES));
        }
        return Ok(texts.join("\n\n"));
    }
    if let Some(content) = read("content.xml")? {
        return Ok(xml_text(&content, &OPEN_DOCUMENT));
    }
    Err(crate::ContentError("the archive is not an OOXML or OpenDocument file this extracts from (docx, xlsx, pptx, odt, ods, odp)".to_owned()))
}
