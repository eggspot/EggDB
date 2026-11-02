const MAX_NAME_BYTES: usize = 255;

/// What a file listed in a multi-part append-only file's manifest holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofFileKind {
    /// The base file (`type b`): the data set when the log was last rewritten.
    Base,
    /// An incremental file (`type i`): the commands logged after the base.
    Incremental,
    /// A history file (`type h`): a file a rewrite replaced, waiting to be deleted.
    History,
}

/// A file a multi-part append-only file's manifest lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofManifestFile {
    /// The file's name, inside the manifest's directory; never a path.
    pub name: String,
    /// Its sequence number.
    pub seq: u64,
    /// What it holds.
    pub kind: AofFileKind,
}

/// The manifest of a multi-part append-only file (Redis 7's `appendonlydir/` with its
/// `appendonly.aof.manifest`), as the files to replay and the history files to skip.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AofManifest {
    /// The files to replay, in order: the base file, then the incremental files by sequence.
    pub replay: Vec<AofManifestFile>,
    /// The history files, which are not replayed.
    pub history: Vec<AofManifestFile>,
}

/// Reads a manifest: one line per file, of key-value pairs such as
/// `file appendonly.aof.1.base.aof seq 1 type b`, `#` lines are comments and unknown keys are
/// ignored. A name that is a path or not a plain file name, a sequence number that is not a
/// number, two base files, two incremental files with one sequence number or no file at all
/// are errors.
pub fn parse_aof_manifest(text: &[u8]) -> Result<AofManifest, String> {
    let mut base = None;
    let mut incrementals: Vec<AofManifestFile> = Vec::new();
    let mut history = Vec::new();
    for (index, line) in text.split(|&byte| byte == b'\n').enumerate() {
        let number = index + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) || line.first() == Some(&b'#') {
            continue;
        }
        let file =
            parse_line(line).map_err(|message| format!("manifest line {number}: {message}"))?;
        match file.kind {
            AofFileKind::Base if base.is_some() => {
                return Err(format!("manifest line {number}: a second base file"));
            }
            AofFileKind::Base => base = Some(file),
            AofFileKind::Incremental => {
                if incrementals.iter().any(|known| known.seq == file.seq) {
                    return Err(format!(
                        "manifest line {number}: two incremental files with seq {}",
                        file.seq
                    ));
                }
                incrementals.push(file);
            }
            AofFileKind::History => history.push(file),
        }
    }
    incrementals.sort_by_key(|file| file.seq);
    let replay: Vec<AofManifestFile> = base.into_iter().chain(incrementals).collect();
    if replay.is_empty() {
        return Err("the manifest lists no base or incremental file".to_owned());
    }
    Ok(AofManifest { replay, history })
}

fn parse_line(line: &[u8]) -> Result<AofManifestFile, String> {
    let tokens = split_tokens(line)?;
    if tokens.len() % 2 != 0 {
        return Err("a key without a value".to_owned());
    }
    let mut name = None;
    let mut seq = None;
    let mut kind = None;
    for pair in tokens.chunks_exact(2) {
        let [key, value] = pair else {
            continue;
        };
        match key.as_slice() {
            b"file" => name = Some(file_name(value)?),
            b"seq" => seq = Some(sequence(value)?),
            b"type" => kind = Some(file_kind(value)?),
            _ => {}
        }
    }
    match (name, seq, kind) {
        (Some(name), Some(seq), Some(kind)) => Ok(AofManifestFile { name, seq, kind }),
        (None, _, _) => Err("no file name".to_owned()),
        (_, None, _) => Err("no seq".to_owned()),
        (_, _, None) => Err("no type".to_owned()),
    }
}

fn file_name(value: &[u8]) -> Result<String, String> {
    let shown = String::from_utf8_lossy(value).escape_debug().to_string();
    let name = std::str::from_utf8(value)
        .map_err(|_| format!("the file name {shown} is not UTF-8"))?;
    let plain = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':') || character.is_control());
    if plain {
        Ok(name.to_owned())
    } else {
        Err(format!(
            "the file name {shown} is not a plain file name in the manifest's directory"
        ))
    }
}

fn sequence(value: &[u8]) -> Result<u64, String> {
    std::str::from_utf8(value)
        .ok()
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| {
            format!(
                "seq {} is not a number of at most 20 digits",
                String::from_utf8_lossy(value).escape_debug()
            )
        })
}

fn file_kind(value: &[u8]) -> Result<AofFileKind, String> {
    match value {
        b"b" => Ok(AofFileKind::Base),
        b"i" => Ok(AofFileKind::Incremental),
        b"h" => Ok(AofFileKind::History),
        _ => Err(format!(
            "type {} is not b, i or h",
            String::from_utf8_lossy(value).escape_debug()
        )),
    }
}

fn split_tokens(line: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut tokens = Vec::new();
    let mut rest = line;
    loop {
        rest = trim_start(rest);
        let Some(&first) = rest.first() else {
            return Ok(tokens);
        };
        let (token, after) = if first == b'"' {
            quoted_token(rest.get(1..).unwrap_or_default())?
        } else {
            let end = rest
                .iter()
                .position(u8::is_ascii_whitespace)
                .unwrap_or(rest.len());
            let (token, after) = rest.split_at(end);
            (token.to_vec(), after)
        };
        tokens.push(token);
        rest = after;
    }
}

fn trim_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

fn quoted_token(mut rest: &[u8]) -> Result<(Vec<u8>, &[u8]), String> {
    let mut token = Vec::new();
    loop {
        match rest {
            [] => return Err("a quoted value without its closing quote".to_owned()),
            [b'"', after @ ..] => {
                if after.first().is_some_and(|byte| !byte.is_ascii_whitespace()) {
                    return Err("a closing quote not followed by a space".to_owned());
                }
                return Ok((token, after));
            }
            [b'\\', b'x', high, low, after @ ..]
                if high.is_ascii_hexdigit() && low.is_ascii_hexdigit() =>
            {
                token.push(hex_value(*high) * 16 + hex_value(*low));
                rest = after;
            }
            [b'\\', escaped, after @ ..] => {
                token.push(match escaped {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'b' => 0x08,
                    b'a' => 0x07,
                    other => *other,
                });
                rest = after;
            }
            [byte, after @ ..] => {
                token.push(*byte);
                rest = after;
            }
        }
    }
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(files: &[AofManifestFile]) -> Vec<&str> {
        files.iter().map(|file| file.name.as_str()).collect()
    }

    #[test]
    fn a_manifest_lists_the_base_then_the_incremental_files_by_seq() {
        let manifest = parse_aof_manifest(
            b"file appendonly.aof.1.base.aof seq 1 type b\n\
              file appendonly.aof.0.base.aof seq 0 type h\n\
              # a comment\n\
              \n\
              file appendonly.aof.3.incr.aof seq 3 type i startoffset 10 endoffset 20\r\n\
              type i seq 2 file \"appendonly.aof.2.incr.aof\"\n",
        )
        .unwrap();
        assert_eq!(
            names(&manifest.replay),
            [
                "appendonly.aof.1.base.aof",
                "appendonly.aof.2.incr.aof",
                "appendonly.aof.3.incr.aof"
            ]
        );
        assert_eq!(names(&manifest.history), ["appendonly.aof.0.base.aof"]);
        assert_eq!(manifest.replay[0].kind, AofFileKind::Base);
    }

    #[test]
    fn quoted_names_unescape() {
        let manifest =
            parse_aof_manifest(b"file \"my file\\x2eaof\" seq 1 type i\n").unwrap();
        assert_eq!(names(&manifest.replay), ["my file.aof"]);
    }

    #[test]
    fn hostile_manifests_are_errors() {
        let refused: [&[u8]; 16] = [
            b"",
            b"# only a comment\n",
            b"file ../etc/passwd seq 1 type i\n",
            b"file /etc/passwd seq 1 type i\n",
            b"file ..\\x2f..\\x2fsecret seq 1 type i\n",
            b"file \"..\" seq 1 type i\n",
            b"file C:evil seq 1 type i\n",
            b"file a\\\\b seq 1 type i\n",
            b"file a seq 99999999999999999999999 type i\n",
            b"file a seq -1 type i\n",
            b"file a seq 1 type x\n",
            b"file a seq 1\n",
            b"file a seq 1 type\n",
            b"file \"a seq 1 type i\n",
            b"file a seq 1 type b\nfile b seq 2 type b\n",
            b"file a seq 1 type i\nfile b seq 1 type i\n",
        ];
        for manifest in refused {
            assert!(
                parse_aof_manifest(manifest).is_err(),
                "{}",
                String::from_utf8_lossy(manifest)
            );
        }
        let long = format!("file {} seq 1 type i\n", "a".repeat(MAX_NAME_BYTES + 1));
        assert!(parse_aof_manifest(long.as_bytes()).is_err());
        assert!(parse_aof_manifest(b"file \xff seq 1 type i\n").is_err());
    }
}
