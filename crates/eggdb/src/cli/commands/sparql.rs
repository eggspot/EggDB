use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};

const BASE: &str = "http://eggdb.local/";
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
