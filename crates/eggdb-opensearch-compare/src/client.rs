use eggdb_http::{HttpTarget, parse_http_url, request};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BODY: usize = 64 << 20;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = chunk
            .iter()
            .enumerate()
            .fold(0u32, |packed, (at, byte)| packed | u32::from(*byte) << (16 - 8 * at));
        for at in 0..4 {
            if at <= chunk.len() {
                let index = (packed >> (18 - 6 * at)) & 63;
                out.push(char::from(BASE64[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[derive(Debug)]
pub(crate) struct Server {
    target: HttpTarget,
    headers: Vec<(String, String)>,
}

impl Server {
    pub(crate) fn reachable(url: &str, credentials: Option<(&str, &str)>) -> Result<Self, String> {
        let target = parse_http_url(url)?;
        let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        if let Some((user, password)) = credentials {
            headers.push((
                "Authorization".to_owned(),
                format!("Basic {}", base64(format!("{user}:{password}").as_bytes())),
            ));
        }
        let server = Self { target, headers };
        server.call("GET", "/", "")?;
        Ok(server)
    }

    pub(crate) fn call(&self, method: &str, path: &str, body: &str) -> Result<String, String> {
        let response = request(&self.target, method, path, &self.headers, body.as_bytes(), TIMEOUT, MAX_BODY)
            .map_err(|error| format!("{method} {path}: {error}"))?;
        let text = String::from_utf8_lossy(&response.body).into_owned();
        if response.status >= 300 {
            return Err(format!("{method} {path}: status {} {text}", response.status));
        }
        Ok(text)
    }

    pub(crate) fn bulk(&self, path: &str, body: &str) -> Result<(), String> {
        let answer = self.call("POST", path, body)?;
        if answer.contains("\"errors\":true") {
            return Err(format!("POST {path}: some bulk items failed"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads_as_rfc_4648_does() {
        assert_eq!(base64(b"eggdb:pw"), "ZWdnZGI6cHc=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
