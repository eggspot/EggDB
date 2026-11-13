use super::Session;
use super::sparql::is_query;
use crate::cli::args::{CommandName, RdfVerb};
use crate::cli::failure::{Exit, Failure};

impl Session<'_> {
    pub(super) fn rdf_command(&mut self) -> Result<Exit, Failure> {
        let usage = |message: &str| Failure::usage(message.to_owned(), Some(CommandName::Rdf));
        let Some((verb, sparql)) = self.request.rdf.clone() else {
            return Err(usage("rdf needs get or run"));
        };
        match (verb, is_query(&sparql)) {
            (RdfVerb::Get, false) => Err(usage("rdf get runs a SPARQL query (SELECT, ASK, CONSTRUCT or DESCRIBE); run updates with eggdb rdf run")),
            (RdfVerb::Run, true) => Err(usage("rdf run runs a SPARQL update; run queries with eggdb rdf get")),
            _ => self.sql_as_sparql(&sparql),
        }
    }
}
