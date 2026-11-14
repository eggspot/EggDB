//! The SAML sign-in pages of the REST listener (D1188): `GET /auth/saml/login` sends the
//! browser to the identity provider the first `saml` rule names, and `POST /auth/saml/acs` takes
//! the provider's signed response and, for a stored role with `LOGIN`, shows a one-time ticket
//! to use as the role's password in psql, the GUI or any other tool for the next five minutes.

use crate::Endpoint;
use crate::request::percent_decode;
use eggdb_http::{Request, Response};
use eggdb_sql::KeyValueStore;

fn html(status: u16, title: &str, body: &str) -> Response {
    Response {
        status,
        content_type: "text/html; charset=utf-8".to_owned(),
        body: format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><h1>{title}</h1>{body}</body></html>"
        )
        .into_bytes(),
        headers: vec![
            ("Cache-Control".to_owned(), "no-store".to_owned()),
            ("Content-Security-Policy".to_owned(), "default-src 'none'".to_owned()),
        ],
    }
}

fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

fn random_bytes<const N: usize>() -> Option<[u8; N]> {
    let mut bytes = [0u8; N];
    eggdb_platform::secure_random::fill(&mut bytes).ok()?;
    Some(bytes)
}

fn form_field(body: &[u8], name: &str) -> Option<String> {
    std::str::from_utf8(body)
        .ok()?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| percent_decode(&value.replace('+', " ")))
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    /// Answers the SAML sign-in paths, or `None` for any other path.
    pub(crate) fn saml_page(&self, path: &str, request: &Request) -> Option<Response> {
        if path != "auth/saml/login" && path != "auth/saml/acs" {
            return None;
        }
        let Some(config) = self.auth_rules.as_ref().and_then(|rules| rules.saml()) else {
            return Some(html(404, "SAML sign-in is not set up", "<p>No <code>saml</code> rule names an identity provider.</p>"));
        };
        Some(match (path, request.method.as_str()) {
            ("auth/saml/login", "GET") => {
                let Some(id) = random_bytes::<16>() else {
                    return Some(html(500, "Sign-in failed", "<p>The system's random source failed.</p>"));
                };
                let request_id = format!("_{}", id.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
                let location = config.sign_in_url(&request_id, now(), "eggdb");
                let mut response = html(302, "Signing in", "<p>Redirecting to your identity provider.</p>");
                response.headers.push(("Location".to_owned(), location));
                response
            }
            ("auth/saml/acs", "POST") => {
                let Some(posted) = form_field(&request.body, "SAMLResponse") else {
                    return Some(html(400, "Sign-in failed", "<p>The identity provider sent no SAMLResponse.</p>"));
                };
                match config.accept_response(&posted, now()) {
                    Err(error) => html(403, "Sign-in failed", &format!("<p>{}</p>", escaped(&error.to_string()))),
                    Ok(login) => {
                        let may_log_in = self
                            .store
                            .begin_read()
                            .ok()
                            .and_then(|reader| eggdb_sql::stored_role(&reader, &login.name).ok().flatten())
                            .is_some_and(|role| role.login);
                        if !may_log_in {
                            return Some(html(403, "Sign-in failed", &format!("<p>No stored role with LOGIN is named {}.</p>", escaped(&login.name))));
                        }
                        let Some(random) = random_bytes::<32>() else {
                            return Some(html(500, "Sign-in failed", "<p>The system's random source failed.</p>"));
                        };
                        let ticket = config.issue_ticket(&login.name, random, now());
                        html(
                            200,
                            "Signed in",
                            &format!(
                                "<p>You are signed in as <strong>{}</strong>. Use this one-time ticket as your password within five minutes:</p><pre>{}</pre><p>For example: <code>psql \"host=… user={} sslmode=require password={}\"</code></p>",
                                escaped(&login.name),
                                escaped(&ticket),
                                escaped(&login.name),
                                escaped(&ticket)
                            ),
                        )
                    }
                }
            }
            _ => html(405, "Not allowed", "<p>Use GET for /auth/saml/login and POST for /auth/saml/acs.</p>"),
        })
    }
}
