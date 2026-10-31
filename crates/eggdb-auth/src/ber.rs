pub(crate) const SEQUENCE: u8 = 0x30;
pub(crate) const INTEGER: u8 = 0x02;
pub(crate) const OCTET_STRING: u8 = 0x04;
pub(crate) const ENUMERATED: u8 = 0x0a;
pub(crate) const BIND_REQUEST: u8 = 0x60;
pub(crate) const BIND_RESPONSE: u8 = 0x61;
pub(crate) const SIMPLE_AUTHENTICATION: u8 = 0x80;
pub(crate) const UNBIND_REQUEST: u8 = 0x42;

pub(crate) fn element(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let length = content.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        let bytes = length.to_be_bytes();
        let skip = bytes.iter().take_while(|byte| **byte == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

pub(crate) fn integer(value: u32) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let mut skip = bytes.iter().take_while(|byte| **byte == 0).count().min(3);
    if bytes[skip] & 0x80 != 0 && skip > 0 {
        skip -= 1;
    }
    let content: Vec<u8> = if bytes[skip] & 0x80 != 0 {
        std::iter::once(0).chain(bytes[skip..].iter().copied()).collect()
    } else {
        bytes[skip..].to_vec()
    };
    element(INTEGER, &content)
}

pub(crate) struct Element<'a> {
    pub(crate) tag: u8,
    pub(crate) content: &'a [u8],
}

pub(crate) fn read(bytes: &[u8]) -> Option<(Element<'_>, &[u8])> {
    let (&tag, rest) = bytes.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 {
            return None;
        }
        let digits = rest.get(..count)?;
        let length = digits
            .iter()
            .fold(0usize, |total, digit| (total << 8) | usize::from(*digit));
        (length, &rest[count..])
    };
    let content = rest.get(..length)?;
    Some((Element { tag, content }, &rest[length..]))
}

pub(crate) fn header_length(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.get(1)?;
    if first < 0x80 {
        return Some(2 + usize::from(first));
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > 4 {
        return None;
    }
    let digits = bytes.get(2..2 + count)?;
    let length = digits
        .iter()
        .fold(0usize, |total, digit| (total << 8) | usize::from(*digit));
    Some(2 + count + length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_and_integers_encode_minimally() {
        assert_eq!(integer(0), [INTEGER, 1, 0]);
        assert_eq!(integer(3), [INTEGER, 1, 3]);
        assert_eq!(integer(128), [INTEGER, 2, 0, 128]);
        assert_eq!(integer(256), [INTEGER, 2, 1, 0]);
        let long = element(OCTET_STRING, &[7; 300]);
        assert_eq!(&long[..4], &[OCTET_STRING, 0x82, 1, 44]);
        let (parsed, rest) = read(&long).unwrap();
        assert_eq!((parsed.content.len(), rest.len()), (300, 0));
        assert_eq!(header_length(&long), Some(304));
        assert!(read(&[OCTET_STRING, 0x85, 1, 1, 1, 1, 1]).is_none());
        assert!(read(&[OCTET_STRING, 5, 1]).is_none());
    }
}
