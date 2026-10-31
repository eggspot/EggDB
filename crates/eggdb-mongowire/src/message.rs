use eggdb_bson::{Bson, Document, read_document};

/// The largest message this server reads or writes, as MongoDB documents its own limit.
pub const MAX_MESSAGE_BYTES: usize = 48_000_000;
pub(crate) const HEADER_BYTES: usize = 16;

pub(crate) const OP_REPLY: i32 = 1;
pub(crate) const OP_QUERY: i32 = 2004;
pub(crate) const OP_MSG: i32 = 2013;

const CHECKSUM_PRESENT: u32 = 1;
const MORE_TO_COME: u32 = 1 << 1;
const QUERY_FAILURE: i32 = 1 << 1;

/// A message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) length: usize,
    pub(crate) request_id: i32,
    pub(crate) op_code: i32,
}

/// A request the server answers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Request {
    Message { body: Document, more_to_come: bool },
    Query { namespace: String, query: Document },
}

fn le_i32(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) fn read_header(bytes: &[u8; HEADER_BYTES]) -> Result<Header, String> {
    let length = le_i32(bytes, 0).unwrap_or(0);
    let length = usize::try_from(length).ok().filter(|length| (HEADER_BYTES..=MAX_MESSAGE_BYTES).contains(length));
    let Some(length) = length else {
        return Err("the message length is out of range".to_owned());
    };
    Ok(Header {
        length,
        request_id: le_i32(bytes, 4).unwrap_or(0),
        op_code: le_i32(bytes, 12).unwrap_or(0),
    })
}

fn cstring(bytes: &[u8], at: usize) -> Result<(String, usize), String> {
    let rest = bytes.get(at..).ok_or("a name runs past the message")?;
    let end = rest.iter().position(|byte| *byte == 0).ok_or("a name is not terminated")?;
    let text = std::str::from_utf8(&rest[..end]).map_err(|_| "a name is not UTF-8".to_owned())?;
    Ok((text.to_owned(), at + end + 1))
}

fn document_at(bytes: &[u8], at: usize) -> Result<(Document, usize), String> {
    let (document, used) = read_document(bytes.get(at..).ok_or("a document runs past the message")?).map_err(|error| error.to_string())?;
    Ok((document, at + used))
}

/// Reads an `OP_MSG` body (the bytes after the header): its sections become one command
/// document, each document sequence an array under its identifier.
pub(crate) fn read_message(body: &[u8]) -> Result<Request, String> {
    let flags = le_i32(body, 0).ok_or("the message has no flags")? as u32;
    let unknown_required = flags & 0xffff & !(CHECKSUM_PRESENT | MORE_TO_COME);
    if unknown_required != 0 {
        return Err(format!("the message sets unknown required flag bits {unknown_required:#x}"));
    }
    let end = if flags & CHECKSUM_PRESENT != 0 {
        body.len().checked_sub(4).ok_or("the message has no room for its checksum")?
    } else {
        body.len()
    };
    let sections = &body[..end];
    let mut at = 4;
    let mut command: Option<Document> = None;
    let mut sequences: Vec<(String, Vec<Bson>)> = Vec::new();
    while at < sections.len() {
        let kind = sections[at];
        at += 1;
        match kind {
            0 => {
                if command.is_some() {
                    return Err("the message has two body sections".to_owned());
                }
                let (document, next) = document_at(sections, at)?;
                command = Some(document);
                at = next;
            }
            1 => {
                let size = le_i32(sections, at).ok_or("a document sequence has no size")?;
                let size = usize::try_from(size).ok().filter(|size| *size >= 5).ok_or("a document sequence has a bad size")?;
                let stop = at.checked_add(size).filter(|stop| *stop <= sections.len()).ok_or("a document sequence runs past the message")?;
                let (identifier, mut next) = cstring(&sections[..stop], at + 4)?;
                let mut documents = Vec::new();
                while next < stop {
                    let (document, after) = document_at(&sections[..stop], next)?;
                    documents.push(Bson::Document(document));
                    next = after;
                }
                sequences.push((identifier, documents));
                at = stop;
            }
            other => return Err(format!("the message has an unknown section kind {other}")),
        }
    }
    let mut command = command.ok_or("the message has no body section")?;
    for (identifier, documents) in sequences {
        if command.contains_key(&identifier) {
            return Err(format!("the field {identifier} is both in the body and a document sequence"));
        }
        command.insert(identifier, Bson::Array(documents));
    }
    Ok(Request::Message { body: command, more_to_come: flags & MORE_TO_COME != 0 })
}

/// Reads a legacy `OP_QUERY` body, as drivers send their first `hello` in.
pub(crate) fn read_query(body: &[u8]) -> Result<Request, String> {
    let (namespace, at) = cstring(body, 4)?;
    let (mut query, _) = document_at(body, at + 8)?;
    if let Some(Bson::Document(inner)) = query.get("$query").or_else(|| query.get("query")).cloned()
        && query.first().is_some_and(|(name, _)| name == "$query" || name == "query")
    {
        query = inner;
    }
    Ok(Request::Query { namespace, query })
}

fn header(out: &mut Vec<u8>, request_id: i32, response_to: i32, op_code: i32) {
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&response_to.to_le_bytes());
    out.extend_from_slice(&op_code.to_le_bytes());
}

fn finish(mut out: Vec<u8>) -> Vec<u8> {
    let length = out.len() as i32;
    out[..4].copy_from_slice(&length.to_le_bytes());
    out
}

/// An `OP_MSG` reply holding `document`.
pub(crate) fn message_reply(request_id: i32, response_to: i32, document: &Document) -> Vec<u8> {
    let mut out = Vec::new();
    header(&mut out, request_id, response_to, OP_MSG);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    document.write(&mut out);
    finish(out)
}

/// An `OP_REPLY` holding `document`, marked as a failure when `failed`.
pub(crate) fn query_reply(request_id: i32, response_to: i32, document: &Document, failed: bool) -> Vec<u8> {
    let mut out = Vec::new();
    header(&mut out, request_id, response_to, OP_REPLY);
    out.extend_from_slice(&(if failed { QUERY_FAILURE } else { 0 }).to_le_bytes());
    out.extend_from_slice(&0i64.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&1i32.to_le_bytes());
    document.write(&mut out);
    finish(out)
}
