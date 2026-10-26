use super::failure::Failure;
use std::ffi::OsStr;
use std::io::Read;
use std::path::PathBuf;

const LONGEST_INPUT_BYTES: u64 = eggdb_storage::BTree::MAX_VALUE_BYTES as u64 + 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ByteArgument {
    Bytes(Vec<u8>),
    File(PathBuf),
    Stdin,
}

impl ByteArgument {
    pub(crate) fn parse(argument: &OsStr) -> Result<Self, String> {
        let bytes = argument.as_encoded_bytes();
        if let Some(literal) = bytes.strip_prefix(b"=") {
            return Ok(Self::Bytes(literal.to_vec()));
        }
        if let Some(digits) = bytes.strip_prefix(b"0x") {
            return parse_hex(digits, bytes).map(Self::Bytes);
        }
        if bytes == b"@-" {
            return Ok(Self::Stdin);
        }
        if bytes.starts_with(b"@") {
            let Some(path) = argument.to_str().and_then(|text| text.strip_prefix('@')) else {
                return Err(
                    "the file name after @ is not valid Unicode; give the bytes as 0x hex instead"
                        .to_owned(),
                );
            };
            return Ok(Self::File(PathBuf::from(path)));
        }
        Ok(Self::Bytes(bytes.to_vec()))
    }

    pub(crate) fn resolve(
        self,
        stdin: &mut dyn Read,
        stdin_taken: &mut bool,
    ) -> Result<Vec<u8>, Failure> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            Self::File(path) => {
                std::fs::File::open(&path)
                    .and_then(read_bounded)
                    .map_err(|error| {
                        Failure::usage(format!("cannot read @{}: {error}", path.display()), None)
                    })
            }
            Self::Stdin if *stdin_taken => Err(Failure::usage(
                "stdin can be read only once: give one argument as @- and the others another way",
                None,
            )),
            Self::Stdin => {
                *stdin_taken = true;
                read_bounded(stdin)
                    .map_err(|error| Failure::usage(format!("cannot read stdin: {error}"), None))
            }
        }
    }
}

fn read_bounded(input: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input.take(LONGEST_INPUT_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_hex(digits: &[u8], whole: &[u8]) -> Result<Vec<u8>, String> {
    let shown = super::render::text(whole);
    if let Some(&bad) = digits.iter().find(|digit| hex_value(**digit).is_none()) {
        let bad = match bad {
            0x20..=0x7e => char::from(bad).to_string(),
            _ => format!("\\x{bad:02x}"),
        };
        return Err(format!("not a hex digit in {shown}: '{bad}'"));
    }
    if !digits.len().is_multiple_of(2) {
        return Err(format!("hex needs an even number of digits: {shown}"));
    }
    Ok(digits
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|[high, low]| Some(hex_value(*high)? << 4 | hex_value(*low)?))
        .collect())
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::ByteArgument;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn parse(text: &str) -> Result<ByteArgument, String> {
        ByteArgument::parse(OsStr::new(text))
    }

    #[test]
    fn text_is_taken_as_typed() {
        assert_eq!(parse("user:1"), Ok(ByteArgument::Bytes(b"user:1".to_vec())));
        assert_eq!(parse(""), Ok(ByteArgument::Bytes(Vec::new())));
    }

    #[test]
    fn hex_takes_either_case() {
        assert_eq!(
            parse("0x48656c6C6f"),
            Ok(ByteArgument::Bytes(b"Hello".to_vec()))
        );
        assert_eq!(parse("0x"), Ok(ByteArgument::Bytes(Vec::new())));
    }

    #[test]
    fn hex_errors_say_what_is_wrong() {
        assert_eq!(
            parse("0x4"),
            Err("hex needs an even number of digits: 0x4".to_owned())
        );
        assert_eq!(
            parse("0x4g"),
            Err("not a hex digit in 0x4g: 'g'".to_owned())
        );
    }

    #[test]
    fn an_equals_sign_escapes_the_rest() {
        assert_eq!(parse("=0x41"), Ok(ByteArgument::Bytes(b"0x41".to_vec())));
        assert_eq!(parse("=@x"), Ok(ByteArgument::Bytes(b"@x".to_vec())));
        assert_eq!(parse("=="), Ok(ByteArgument::Bytes(b"=".to_vec())));
        assert_eq!(parse("="), Ok(ByteArgument::Bytes(Vec::new())));
    }

    #[test]
    fn at_names_a_file_or_stdin() {
        assert_eq!(
            parse("@data/blob.bin"),
            Ok(ByteArgument::File(PathBuf::from("data/blob.bin")))
        );
        assert_eq!(parse("@-"), Ok(ByteArgument::Stdin));
    }

    #[test]
    fn stdin_is_read_once() {
        let mut stdin: &[u8] = b"piped";
        let mut taken = false;
        assert_eq!(
            ByteArgument::Stdin.resolve(&mut stdin, &mut taken),
            Ok(b"piped".to_vec())
        );
        assert!(ByteArgument::Stdin.resolve(&mut stdin, &mut taken).is_err());
    }
}
