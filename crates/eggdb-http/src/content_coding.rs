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

    fn decode(self, data: &[u8], limit: usize) -> Result<Vec<u8>, DecompressError> {
        match self {
            Self::Gzip => eggdb_content::gunzip(data, limit),
            Self::Deflate => eggdb_content::zlib_inflate(data, limit),
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
            let budget_left = budget
                .limit_bytes()
                .saturating_sub(budget.used_bytes())
                .max(UNCOUNTED_BUFFER_BYTES);
            let limit = max_body_bytes.min(budget_left);
            let decoded = coding
                .decode(&request.body, limit)
                .map_err(|error| match error {
                    DecompressError::PastLimit(_) if limit < max_body_bytes => {
                        HttpError::OverMemoryBudget
                    }
                    DecompressError::PastLimit(_) => HttpError::BodyTooLarge,
                    DecompressError::Corrupt(_) => HttpError::CorruptContentCoding,
                })?;
            if !held.hold_if_large(request.body.len().saturating_add(decoded.len())) {
                return Err(HttpError::OverMemoryBudget);
            }
            request.body = decoded;
            if !held.hold_if_large(request.body.len()) {
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
