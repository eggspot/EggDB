use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};

const BASE: &str = "http://eggdb.local/";
const LOAD_VARIABLE: &str = "EGGDB_SPARQL_LOAD";
const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MOST_LOADED_BYTES: usize = 256 << 20;
const LOAD_ACCEPT: &str = "text/turtle, application/n-triples, application/n-quads, application/trig;q=0.9, */*;q=0.1";

/// Lets SPARQL's `LOAD` fetch the `http://` and `https://` documents under the
/// comma-separated URL prefixes in `EGGDB_SPARQL_LOAD`; with none set, `LOAD` is refused.
pub(super) fn allow_load_from_environment() {
    let Ok(text) = std::env::var(LOAD_VARIABLE) else {
        return;
    };
    let allowed: Vec<String> = text.split(',').map(str::trim).filter(|entry| !entry.is_empty()).map(str::to_owned).collect();
    if allowed.is_empty() {
        return;
    }
    eggdb_rdf::set_remote_fetch(Box::new(move |iri| {
        if !allowed.iter().any(|entry| super::foreign::web_allowed(entry, iri)) {
            return Err(format!("{iri} is not under a prefix {LOAD_VARIABLE} allows"));
        }
        let target = eggdb_http::parse_http_url(iri)?;
        let headers = vec![("Accept".to_owned(), LOAD_ACCEPT.to_owned())];
        let response = eggdb_http::request(&target, "GET", &target.path, &headers, &[], LOAD_TIMEOUT, MOST_LOADED_BYTES)
            .map_err(|error| format!("cannot fetch it: {error}"))?;
        if response.status != 200 {
            return Err(format!("the server answered HTTP {}", response.status));
        }
        let media_type = response.header("content-type").unwrap_or_default().to_owned();
        let body = String::from_utf8(response.body).map_err(|_| "the document is not UTF-8".to_owned())?;
        Ok((media_type, body))
    }));
}
const QUERY_FORMS: [&str; 4] = ["select", "ask", "construct", "describe"];

fn is_query(text: &str) -> bool {
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let lowered = word.to_ascii_lowercase();
        match lowered.as_str() {
            "prefix" => {
                words.next();
                words.next();
            }
            "base" => {
                words.next();
            }
            first => return QUERY_FORMS.iter().any(|form| first.starts_with(form)),
        }
    }
    false
}

impl Session<'_> {
    pub(super) fn sql_as_sparql(&mut self, text: &str) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Sql));
        let query = is_query(text);
        if !query && self.request.read_only {
            return Err(usage("a SPARQL update writes: give it without --read-only".to_owned()));
        }
        let access = if query {
            Access::ReadOnly
        } else {
            Access::Write { create: self.request.create }
        };
        let db = self.open(access, None)?;
        let keyspace = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::RDF_COLLECTIONS,
        };
        let outcome = if query {
            eggdb_rdf::query(&keyspace, 0, text, BASE).map(|answer| {
                let format = if self.format == Format::Json {
                    eggdb_rdf::ResultFormat::Json
                } else {
                    eggdb_rdf::ResultFormat::Tsv
                };
                eggdb_rdf::write_results(&answer, format).1
            })
        } else {
            eggdb_rdf::update(&keyspace, 0, text, BASE).map(|summary| {
                if self.format == Format::Json {
                    format!("{{\"inserted\":{},\"deleted\":{}}}\n", summary.inserted, summary.deleted)
                } else {
                    format!("inserted {} and deleted {} quads\n", summary.inserted, summary.deleted)
                }
            })
        };
        drop(keyspace);
        self.close(db)?;
        match outcome {
            Ok(written) => {
                self.out.text(&written);
                if !written.ends_with('\n') {
                    self.out.text("\n");
                }
                Ok(Exit::Done)
            }
            Err(error) => Err(Failure::database(format!("SPARQL: {error}"), Exit::DatabaseError)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_query;

    #[test]
    fn queries_and_updates_are_told_apart_after_the_prologue() {
        assert!(is_query("PREFIX ex: <http://ex/> SELECT * WHERE { ?s ?p ?o }"));
        assert!(is_query("BASE <http://ex/> ask { ?s ?p ?o }"));
        assert!(is_query("SELECT* WHERE {}"));
        assert!(!is_query("PREFIX ex: <http://ex/> INSERT DATA { ex:a ex:b ex:c }"));
        assert!(!is_query("CLEAR ALL"));
    }
}
