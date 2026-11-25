use super::{Access, Session};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use eggdb::Database;

const IN_PROCESS_USER: &str = "eggdb";
const EVERY_HIT: &str = "{}";

impl Session<'_> {
    pub(super) fn search_command(&mut self) -> Result<Exit, Failure> {
        let Some((index, body)) = self.request.search.clone() else {
            return Err(Failure::usage("search needs get", Some(CommandName::Search)));
        };
        let db = self.open(Access::ReadOnly, None)?;
        let outcome = search_index(&db, &index, body.as_deref().unwrap_or(EVERY_HIT));
        self.close(db)?;
        match outcome {
            Ok(response) => {
                self.out.text(response.trim_end());
                self.out.text("\n");
                Ok(Exit::Done)
            }
            Err(message) => Err(Failure::coded("search_error", message, Exit::DatabaseError)),
        }
    }
}

/// Runs one `_search` request body on `index` of the file's search indices, as
/// `eggdb serve --es-listen` would answer `POST /<index>/_search`, and returns its JSON
/// response; a request the API refuses is its error's reason.
pub(super) fn search_index(db: &Database, index: &str, body: &str) -> Result<String, String> {
    let plain = !index.is_empty()
        && !index.contains(|character: char| "/?#%".contains(character) || character.is_whitespace());
    if !plain {
        return Err(format!("{index:?} is not an index name"));
    }
    let keyspace = super::serve_resp::DatabaseKeyspace {
        db,
        prefix: super::serve_resp::ES_COLLECTIONS,
    };
    let password = eggdb_crypto::base64_encode(&in_process_secret());
    let api = eggdb_esrest::Api::new(&keyspace, IN_PROCESS_USER, eggdb_crypto::sha256(password.as_bytes()));
    let credentials = eggdb_crypto::base64_encode(format!("{IN_PROCESS_USER}:{password}").as_bytes());
    let request = eggdb_http::Request {
        method: "POST".to_owned(),
        path: format!("/{index}/_search"),
        query: None,
        headers: vec![
            ("authorization".to_owned(), format!("Basic {credentials}")),
            ("content-type".to_owned(), "application/json".to_owned()),
        ],
        body: body.as_bytes().to_vec(),
    };
    let response = api.answer(&request);
    let text = String::from_utf8_lossy(&response.body).into_owned();
    if response.status == 200 {
        Ok(text)
    } else {
        Err(format!("HTTP {}: {}", response.status, text.trim()))
    }
}

fn in_process_secret() -> [u8; 16] {
    let mut secret = [0u8; 16];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    secret.copy_from_slice(&now.to_le_bytes());
    secret
}
