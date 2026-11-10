use crate::{HttpError, Request};
use eggdb_content::DecompressError;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation, UNCOUNTED_BUFFER_BYTES};

const MAX_STACKED_CODINGS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentCoding {
    Gzip,
    Deflate,
}

impl ContentCoding {
    fn named(name: &str) -> Result<Option<Self>, HttpError> {
        if name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip") {
            Ok(Some(Self::Gzip))
        } else if name.eq_ignore_ascii_case("deflate") {
            Ok(Some(Self::Deflate))
        } else if name.eq_ignore_ascii_case("identity") {
            Ok(None)
        } else {
            Err(HttpError::UnsupportedContentCoding)
        }
    }

    fn decode(
        self,
        data: &[u8],
        limit: usize,
        memory_bytes: usize,
    ) -> Result<Vec<u8>, DecompressError> {
        match self {
            Self::Gzip => eggdb_content::gunzip_within_memory(data, limit, memory_bytes),
            Self::Deflate => eggdb_content::zlib_inflate_within_memory(data, limit, memory_bytes),
        }
    }
}

fn is_content_encoding(name: &str) -> bool {
    name == "content-encoding"
}

fn codings_in_applied_order(request: &Request) -> Result<Vec<ContentCoding>, HttpError> {
    let mut codings = Vec::new();
    for name in request
        .headers
        .iter()
        .filter(|(name, _)| is_content_encoding(name))
        .flat_map(|(_, value)| value.split(','))
        .map(|name| name.trim_matches([' ', '\t']))
        .filter(|name| !name.is_empty())
    {
        if let Some(coding) = ContentCoding::named(name)? {
            if codings.len() == MAX_STACKED_CODINGS {
                return Err(HttpError::UnsupportedContentCoding);
            }
            codings.push(coding);
        }
    }
    Ok(codings)
}

fn hold_memory_for_decoding(
    held: &mut Reservation<'_>,
    budget: &MemoryBudget,
    compressed: usize,
    max_body_bytes: usize,
) -> usize {
    if held.hold_if_large(compressed.saturating_add(max_body_bytes)) {
        return max_body_bytes;
    }
    let room = budget
        .limit_bytes()
        .saturating_sub(budget.used_bytes())
        .saturating_add(held.bytes())
        .max(UNCOUNTED_BUFFER_BYTES);
    let memory_bytes = room.saturating_sub(compressed).min(max_body_bytes);
    if held.hold_if_large(compressed.saturating_add(memory_bytes)) {
        memory_bytes
    } else {
        0
    }
}

pub(crate) fn decode_request_body(
    request: &mut Request,
    max_body_bytes: usize,
    held: &mut Reservation<'_>,
    budget: &MemoryBudget,
) -> Result<(), HttpError> {
    if !request
        .headers
        .iter()
        .any(|(name, _)| is_content_encoding(name))
    {
        return Ok(());
    }
    let codings = codings_in_applied_order(request)?;
    if !request.body.is_empty() {
        for coding in codings.iter().rev() {
            let compressed = request.body.len();
            let memory_bytes = hold_memory_for_decoding(held, budget, compressed, max_body_bytes);
            let decoded = coding
                .decode(&request.body, max_body_bytes, memory_bytes)
                .map_err(|error| match error {
                    DecompressError::PastLimit(_) => HttpError::BodyTooLarge,
                    DecompressError::PastMemory(_) => HttpError::OverMemoryBudget,
                    DecompressError::Corrupt(_) => HttpError::CorruptContentCoding,
                })?;
            if !held.hold_if_large(compressed.saturating_add(decoded.capacity())) {
                return Err(HttpError::OverMemoryBudget);
            }
            request.body = decoded;
            if !held.hold_if_large(request.body.capacity()) {
                return Err(HttpError::OverMemoryBudget);
            }
        }
    }
    request.headers.retain(|(name, _)| {
        !matches!(
            name.as_str(),
            "content-encoding" | "content-length" | "transfer-encoding"
        )
    });
    request
        .headers
        .push(("content-length".to_owned(), request.body.len().to_string()));
    Ok(())
}
