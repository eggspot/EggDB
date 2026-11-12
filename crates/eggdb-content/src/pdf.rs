use crate::ContentError;
use std::collections::BTreeMap;

const MOST_DEPTH: usize = 64;
const MOST_PAGES: usize = 100_000;
const WORD_GAP: f64 = -200.0;

fn damaged(message: &str) -> ContentError {
    ContentError(format!("the PDF file is damaged: {message}"))
}

#[derive(Debug, Clone, PartialEq)]
enum Object {
    Null,
    Boolean(bool),
    Number(f64),
    Text(Vec<u8>),
    Name(String),
    Array(Vec<Object>),
    Dictionary(Vec<(String, Object)>),
    Reference(u32),
    Stream(Vec<(String, Object)>, Vec<u8>),
    Keyword(String),
}

impl Object {
    fn get(&self, key: &str) -> Option<&Object> {
        match self {
            Object::Dictionary(entries) | Object::Stream(entries, _) => {
                entries.iter().find(|(name, _)| name == key).map(|(_, value)| value)
            }
            _ => None,
        }
    }
}

fn is_delimiter(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

struct Lexer<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Lexer<'b> {
    fn skip_space(&mut self) {
        while let Some(&byte) = self.bytes.get(self.at) {
            if byte.is_ascii_whitespace() || byte == 0 {
                self.at += 1;
            } else if byte == b'%' {
                while self.bytes.get(self.at).is_some_and(|byte| *byte != b'\n' && *byte != b'\r') {
                    self.at += 1;
                }
            } else {
                return;
            }
        }
    }

    fn word(&mut self) -> &'b [u8] {
        let start = self.at;
        while let Some(&byte) = self.bytes.get(self.at) {
            if byte.is_ascii_whitespace() || is_delimiter(byte) || byte == 0 {
                break;
            }
            self.at += 1;
        }
        &self.bytes[start..self.at]
    }

    fn literal_string(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut depth = 1usize;
        self.at += 1;
        while let Some(&byte) = self.bytes.get(self.at) {
            self.at += 1;
            match byte {
                b'(' => {
                    depth += 1;
                    out.push(byte);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(byte);
                }
                b'\\' => {
                    let Some(&escaped) = self.bytes.get(self.at) else { break };
                    self.at += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'\r' => {
                            if self.bytes.get(self.at) == Some(&b'\n') {
                                self.at += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut value = u32::from(escaped - b'0');
                            for _ in 0..2 {
                                match self.bytes.get(self.at) {
                                    Some(&digit @ b'0'..=b'7') => {
                                        value = value * 8 + u32::from(digit - b'0');
                                        self.at += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(value as u8);
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    fn hex_string(&mut self) -> Vec<u8> {
        self.at += 1;
        let mut digits = Vec::new();
        while let Some(&byte) = self.bytes.get(self.at) {
            self.at += 1;
            if byte == b'>' {
                break;
            }
            if let Some(digit) = (byte as char).to_digit(16) {
                digits.push(digit as u8);
            }
        }
        if digits.len() % 2 == 1 {
            digits.push(0);
        }
        digits.chunks(2).map(|pair| pair[0] << 4 | pair[1]).collect()
    }

    fn object(&mut self, depth: usize) -> Option<Object> {
        if depth > MOST_DEPTH {
            return None;
        }
        self.skip_space();
        let byte = *self.bytes.get(self.at)?;
        Some(match byte {
            b'(' => Object::Text(self.literal_string()),
            b'<' if self.bytes.get(self.at + 1) == Some(&b'<') => {
                self.at += 2;
                let mut entries = Vec::new();
                loop {
                    self.skip_space();
                    if self.bytes.get(self.at..self.at + 2) == Some(b">>") {
                        self.at += 2;
                        break;
                    }
                    let Some(Object::Name(key)) = self.object(depth + 1) else {
                        return None;
                    };
                    let value = self.object(depth + 1)?;
                    entries.push((key, value));
                }
                Object::Dictionary(entries)
            }
            b'<' => Object::Text(self.hex_string()),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_space();
                    if self.bytes.get(self.at) == Some(&b']') {
                        self.at += 1;
                        break;
                    }
                    items.push(self.object(depth + 1)?);
                }
                Object::Array(items)
            }
            b'/' => {
                self.at += 1;
                let raw = self.word();
                let mut name = Vec::with_capacity(raw.len());
                let mut index = 0;
                while index < raw.len() {
                    if raw[index] == b'#' && index + 2 < raw.len() + 1 {
                        if let Some(value) = std::str::from_utf8(raw.get(index + 1..index + 3).unwrap_or_default()).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                            name.push(value);
                            index += 3;
                            continue;
                        }
                    }
                    name.push(raw[index]);
                    index += 1;
                }
                Object::Name(String::from_utf8_lossy(&name).into_owned())
            }
            b')' | b'>' | b']' | b'{' | b'}' => {
                self.at += 1;
                Object::Keyword(char::from(byte).to_string())
            }
            _ => {
                let word = self.word();
                if word.is_empty() {
                    self.at += 1;
                    return Some(Object::Null);
                }
                let text = String::from_utf8_lossy(word).into_owned();
                if let Ok(number) = text.parse::<f64>() {
                    let saved = self.at;
                    if number.fract() == 0.0 && number >= 0.0 {
                        self.skip_space();
                        let generation = self.word();
                        let is_generation = !generation.is_empty() && generation.iter().all(u8::is_ascii_digit);
                        if is_generation {
                            self.skip_space();
                            if self.word() == b"R" {
                                return Some(Object::Reference(number as u32));
                            }
                        }
                    }
                    self.at = saved;
                    return Some(Object::Number(number));
                }
                match text.as_str() {
                    "true" => Object::Boolean(true),
                    "false" => Object::Boolean(false),
                    "null" => Object::Null,
                    _ => Object::Keyword(text),
                }
            }
        })
    }
}

fn number(object: Option<&Object>) -> Option<f64> {
    match object {
        Some(Object::Number(value)) => Some(*value),
        _ => None,
    }
}

struct Document {
    objects: BTreeMap<u32, Object>,
    limit: usize,
}

impl Document {
    fn resolve<'o>(&'o self, object: &'o Object) -> &'o Object {
        let mut current = object;
        for _ in 0..MOST_DEPTH {
            match current {
                Object::Reference(id) => match self.objects.get(id) {
                    Some(found) => current = found,
                    None => return &Object::Null,
                },
                other => return other,
            }
        }
        &Object::Null
    }

    fn decoded(&self, object: &Object) -> Option<Vec<u8>> {
        let Object::Stream(_, raw) = object else {
            return None;
        };
        let filters: Vec<String> = match object.get("Filter").map(|filter| self.resolve(filter)) {
            Some(Object::Name(name)) => vec![name.clone()],
            Some(Object::Array(items)) => items
                .iter()
                .filter_map(|item| match self.resolve(item) {
                    Object::Name(name) => Some(name.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut data = raw.clone();
        for filter in filters {
            data = match filter.as_str() {
                "FlateDecode" | "Fl" => crate::zlib_inflate(&data, self.limit).ok()?,
                "ASCIIHexDecode" | "AHx" => Lexer { bytes: &[b"<".as_slice(), &data].concat(), at: 0 }.hex_string(),
                _ => return None,
            };
        }
        Some(data)
    }
}

fn read_objects(bytes: &[u8], limit: usize) -> Document {
    let mut objects = BTreeMap::new();
    let mut at = 0;
    while let Some(found) = find(bytes, b"obj", at) {
        at = found + 3;
        let Some(id) = object_number_before(bytes, found) else {
            continue;
        };
        let mut lexer = Lexer { bytes, at };
        let Some(object) = lexer.object(0) else {
            continue;
        };
        lexer.skip_space();
        let object = if let Object::Dictionary(entries) = &object
            && bytes.get(lexer.at..lexer.at + 6) == Some(b"stream")
        {
            let mut start = lexer.at + 6;
            if bytes.get(start) == Some(&b'\r') {
                start += 1;
            }
            if bytes.get(start) == Some(&b'\n') {
                start += 1;
            }
            let declared = entries
                .iter()
                .find(|(key, _)| key == "Length")
                .and_then(|(_, value)| number(Some(value)))
                .map(|length| length as usize)
                .filter(|length| bytes.get(start..start + length + 1).is_some());
            let end = match declared {
                Some(length) => start + length,
                None => {
                    let mut end = find(bytes, b"endstream", start).unwrap_or(bytes.len());
                    while end > start && matches!(bytes[end - 1], b'\r' | b'\n') {
                        end -= 1;
                    }
                    end
                }
            };
            at = end;
            Object::Stream(entries.clone(), bytes[start..end.min(bytes.len())].to_vec())
        } else {
            at = lexer.at;
            object
        };
        objects.insert(id, object);
    }
    let mut document = Document { objects, limit };
    let streams: Vec<Object> = document
        .objects
        .values()
        .filter(|object| matches!(object.get("Type"), Some(Object::Name(kind)) if kind == "ObjStm"))
        .cloned()
        .collect();
    for stream in streams {
        let (Some(count), Some(first)) = (number(stream.get("N")), number(stream.get("First"))) else {
            continue;
        };
        let Some(data) = document.decoded(&stream) else {
            continue;
        };
        let mut header = Lexer { bytes: &data, at: 0 };
        let mut entries = Vec::new();
        for _ in 0..(count as usize).min(data.len()) {
            let (Some(Object::Number(id)), Some(Object::Number(offset))) = (header.object(0), header.object(0)) else {
                break;
            };
            entries.push((id as u32, offset as usize));
        }
        for (id, offset) in entries {
            let mut lexer = Lexer { bytes: &data, at: first as usize + offset };
            if let Some(object) = lexer.object(0) {
                document.objects.entry(id).or_insert(object);
            }
        }
    }
    document
}

fn find(bytes: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    bytes.get(from..)?.windows(needle.len()).position(|window| window == needle).map(|at| at + from)
}

fn object_number_before(bytes: &[u8], at: usize) -> Option<u32> {
    let mut end = at;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == end {
        return None;
    }
    let mut number_end = start;
    while number_end > 0 && bytes[number_end - 1].is_ascii_whitespace() {
        number_end -= 1;
    }
    let mut number_start = number_end;
    while number_start > 0 && bytes[number_start - 1].is_ascii_digit() {
        number_start -= 1;
    }
    std::str::from_utf8(&bytes[number_start..number_end]).ok()?.parse().ok()
}

struct ToUnicode {
    code_bytes: usize,
    map: BTreeMap<u32, String>,
}

fn utf16_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks(2).map(|pair| u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)])).collect();
    String::from_utf16_lossy(&units)
}

fn code_of(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |code, byte| code << 8 | u32::from(*byte))
}

fn to_unicode(data: &[u8]) -> ToUnicode {
    let mut lexer = Lexer { bytes: data, at: 0 };
    let mut map = BTreeMap::new();
    let mut code_bytes = 1;
    let mut pending: Vec<Object> = Vec::new();
    let mut mode = "";
    while let Some(object) = lexer.object(0) {
        if lexer.at >= data.len() && object == Object::Null {
            break;
        }
        match &object {
            Object::Keyword(word) if word == "begincodespacerange" || word == "beginbfchar" || word == "beginbfrange" => {
                mode = if word == "beginbfchar" { "char" } else if word == "beginbfrange" { "range" } else { "space" };
                pending.clear();
            }
            Object::Keyword(word) if word.starts_with("end") => {
                match mode {
                    "space" => {
                        if let Some(Object::Text(low)) = pending.first() {
                            code_bytes = low.len().clamp(1, 4);
                        }
                    }
                    "char" => {
                        for pair in pending.chunks(2) {
                            if let [Object::Text(source), Object::Text(target)] = pair {
                                map.insert(code_of(source), utf16_text(target));
                            }
                        }
                    }
                    "range" => {
                        for triple in pending.chunks(3) {
                            match triple {
                                [Object::Text(low), Object::Text(high), Object::Text(target)] => {
                                    let (low, high) = (code_of(low), code_of(high));
                                    let base = utf16_text(target);
                                    let mut units: Vec<u16> = base.encode_utf16().collect();
                                    for code in low..=high.min(low.saturating_add(65_535)) {
                                        map.insert(code, String::from_utf16_lossy(&units));
                                        if let Some(last) = units.last_mut() {
                                            *last = last.wrapping_add(1);
                                        }
                                    }
                                }
                                [Object::Text(low), Object::Text(_), Object::Array(targets)] => {
                                    let low = code_of(low);
                                    for (offset, target) in targets.iter().enumerate() {
                                        if let Object::Text(target) = target {
                                            map.insert(low + offset as u32, utf16_text(target));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                mode = "";
                pending.clear();
            }
            _ if !mode.is_empty() => pending.push(object),
            _ => {}
        }
        if lexer.at >= data.len() {
            break;
        }
    }
    ToUnicode { code_bytes, map }
}

fn decoded_text(bytes: &[u8], font: Option<&ToUnicode>) -> String {
    match font {
        Some(font) if !font.map.is_empty() => bytes
            .chunks(font.code_bytes)
            .map(|code| font.map.get(&code_of(code)).cloned().unwrap_or_default())
            .collect(),
        _ => bytes.iter().map(|byte| char::from(*byte)).filter(|character| !character.is_control() || *character == '\n').collect(),
    }
}

fn page_text(content: &[u8], fonts: &BTreeMap<String, ToUnicode>, out: &mut String) {
    let mut lexer = Lexer { bytes: content, at: 0 };
    let mut operands: Vec<Object> = Vec::new();
    let mut font: Option<&ToUnicode> = None;
    let newline = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };
    loop {
        lexer.skip_space();
        if lexer.at >= content.len() {
            break;
        }
        let before = lexer.at;
        let Some(object) = lexer.object(0) else {
            break;
        };
        if lexer.at == before {
            lexer.at += 1;
            continue;
        }
        let Object::Keyword(operator) = object else {
            operands.push(object);
            continue;
        };
        match operator.as_str() {
            "BI" => {
                if let Some(end) = find(content, b"EI", lexer.at) {
                    lexer.at = end + 2;
                }
            }
            "Tf" => {
                if let Some(Object::Name(name)) = operands.first() {
                    font = fonts.get(name);
                }
            }
            "Tj" | "'" | "\"" => {
                if operator != "Tj" {
                    newline(out);
                }
                if let Some(Object::Text(text)) = operands.last() {
                    out.push_str(&decoded_text(text, font));
                }
            }
            "TJ" => {
                if let Some(Object::Array(items)) = operands.last() {
                    for item in items {
                        match item {
                            Object::Text(text) => out.push_str(&decoded_text(text, font)),
                            Object::Number(gap) if *gap < WORD_GAP && !out.ends_with(' ') => out.push(' '),
                            _ => {}
                        }
                    }
                }
            }
            "T*" | "ET" => newline(out),
            "Td" | "TD" => {
                if number(operands.get(1)).is_some_and(|ty| ty != 0.0) {
                    newline(out);
                } else if !out.ends_with(' ') && !out.is_empty() && !out.ends_with('\n') {
                    out.push(' ');
                }
            }
            _ => {}
        }
        operands.clear();
    }
    newline(out);
}

fn pages(document: &Document) -> Vec<&Object> {
    let root = document.objects.values().find_map(|object| match object.get("Type") {
        Some(Object::Name(kind)) if kind == "Catalog" => object.get("Pages"),
        _ => None,
    });
    let mut out = Vec::new();
    if let Some(root) = root {
        let mut stack = vec![(document.resolve(root), 0usize)];
        while let Some((node, depth)) = stack.pop() {
            if depth > MOST_DEPTH || out.len() >= MOST_PAGES {
                break;
            }
            match node.get("Type") {
                Some(Object::Name(kind)) if kind == "Page" => out.push(node),
                _ => {
                    if let Some(Object::Array(kids)) = node.get("Kids").map(|kids| document.resolve(kids)) {
                        for kid in kids.iter().rev() {
                            stack.push((document.resolve(kid), depth + 1));
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out = document
            .objects
            .values()
            .filter(|object| matches!(object.get("Type"), Some(Object::Name(kind)) if kind == "Page"))
            .take(MOST_PAGES)
            .collect();
    }
    out
}

fn inherited<'d>(document: &'d Document, page: &'d Object, key: &str) -> Option<&'d Object> {
    let mut node = page;
    for _ in 0..MOST_DEPTH {
        if let Some(found) = node.get(key) {
            return Some(document.resolve(found));
        }
        node = document.resolve(node.get("Parent")?);
    }
    None
}

/// The text of a PDF file, page by page, one line per text line the content streams draw:
/// objects are read directly and from object streams, content streams decoded (Flate and
/// ASCIIHex), and each font's text mapped through its `ToUnicode` CMap when it has one;
/// produces at most `limit` bytes of decoded streams per stream and of text in all.
pub(crate) fn text(bytes: &[u8], limit: usize) -> Result<String, ContentError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(damaged("no %PDF- header"));
    }
    let document = read_objects(bytes, limit);
    let mut out = String::new();
    for page in pages(&document) {
        let mut fonts = BTreeMap::new();
        if let Some(resources) = inherited(&document, page, "Resources")
            && let Some(Object::Dictionary(entries)) = resources.get("Font").map(|fonts| document.resolve(fonts))
        {
            for (name, font) in entries {
                let font = document.resolve(font);
                if let Some(map) = font.get("ToUnicode").and_then(|stream| document.decoded(document.resolve(stream))) {
                    fonts.insert(name.clone(), to_unicode(&map));
                }
            }
        }
        let contents: Vec<&Object> = match page.get("Contents").map(|contents| document.resolve(contents)) {
            Some(Object::Array(items)) => items.iter().map(|item| document.resolve(item)).collect(),
            Some(stream) => vec![stream],
            None => Vec::new(),
        };
        let mut content = Vec::new();
        for stream in contents {
            if let Some(data) = document.decoded(stream) {
                content.extend_from_slice(&data);
                content.push(b'\n');
            }
        }
        page_text(&content, &fonts, &mut out);
        if out.len() > limit {
            return Err(ContentError(format!("the text is longer than {limit} bytes")));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_pages_give_their_text_in_order() {
        let content = b"BT /F1 12 Tf 72 700 Td (Hello) Tj 0 -14 Td [(Wor) -300 (ld)] TJ ET";
        let file = format!(
            "%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Contents 4 0 R >> endobj\n4 0 obj << /Length {} >>\nstream\n{}\nendstream endobj\ntrailer << /Root 1 0 R >>\n%%EOF",
            content.len(),
            String::from_utf8_lossy(content)
        );
        assert_eq!(text(file.as_bytes(), 1 << 20).unwrap(), "Hello\nWor ld\n");
        assert!(text(b"not a pdf", 10).is_err());
    }

    #[test]
    fn to_unicode_maps_two_byte_codes() {
        let cmap = to_unicode(b"1 begincodespacerange <0000> <FFFF> endcodespacerange 1 beginbfchar <0003> <0041> endbfchar 1 beginbfrange <0010> <0012> <0061> endbfrange");
        assert_eq!(decoded_text(&[0, 3, 0, 0x11], Some(&cmap)), "Ab");
    }
}
