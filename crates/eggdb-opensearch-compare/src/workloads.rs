use crate::baseline::Metric;
use crate::client::Server;
use crate::measure;
use std::cell::Cell;

pub(crate) const BULK: &str = "opensearch_compare.bulk";
pub(crate) const GET: &str = "opensearch_compare.get";
pub(crate) const MATCH: &str = "opensearch_compare.match";
pub(crate) const TERMS: &str = "opensearch_compare.terms";

const DOCUMENTS: u64 = 10_000;
const BULK_SIZE: u64 = 100;
const BULK_INDEX: &str = "eggdb_compare_bulk";
const READ_INDEX: &str = "eggdb_compare_read";
const WORDS: [&str; 16] = [
    "egg", "quick", "brown", "fox", "database", "search", "index", "fast", "light", "single", "binary",
    "query", "vector", "graph", "stream", "cache",
];
const MAPPINGS: &str = "{\"mappings\":{\"properties\":{\"title\":{\"type\":\"text\"},\"category\":{\"type\":\"keyword\"},\"price\":{\"type\":\"long\"}}}}";

#[derive(Debug)]
pub(crate) struct Servers {
    pub(crate) eggdb: Server,
    pub(crate) opensearch: Server,
    pub(crate) read_index_loaded: Cell<bool>,
}

fn word(number: u64) -> &'static str {
    WORDS[(number % WORDS.len() as u64) as usize]
}

fn document(number: u64) -> String {
    format!(
        "{{\"title\":\"{} {} {}\",\"category\":\"c{}\",\"price\":{}}}",
        word(number),
        word(number >> 4),
        word(number >> 8),
        number % 20,
        number % 1000
    )
}

fn bulk_body(index: &str, first: u64, count: u64) -> String {
    let mut body = String::new();
    for number in first..first + count {
        body.push_str(&format!(
            "{{\"index\":{{\"_index\":\"{index}\",\"_id\":\"{}\"}}}}\n",
            number % DOCUMENTS
        ));
        body.push_str(&document(number));
        body.push('\n');
    }
    body
}

fn fresh_index(server: &Server, index: &str) -> Result<(), String> {
    let _ = server.call("DELETE", &format!("/{index}"), "");
    server.call("PUT", &format!("/{index}"), MAPPINGS)?;
    Ok(())
}

fn loaded_index(server: &Server, index: &str) -> Result<(), String> {
    fresh_index(server, index)?;
    let mut first = 0;
    while first < DOCUMENTS {
        server.bulk("/_bulk", &bulk_body(index, first, BULK_SIZE))?;
        first += BULK_SIZE;
    }
    server.call("POST", &format!("/{index}/_refresh"), "")?;
    Ok(())
}

impl Servers {
    fn with_read_index(&self) -> Result<(), String> {
        if !self.read_index_loaded.get() {
            loaded_index(&self.eggdb, READ_INDEX)?;
            loaded_index(&self.opensearch, READ_INDEX)?;
            self.read_index_loaded.set(true);
        }
        Ok(())
    }

    fn paired(&self, each: &dyn Fn(&Server, u64) -> Result<(), String>) -> Result<measure::Paired, String> {
        let (mut ours, mut theirs) = (0u64, 0u64);
        measure::paired(
            &mut |iterations| {
                for _ in 0..iterations {
                    each(&self.eggdb, ours)?;
                    ours += 1;
                }
                Ok(())
            },
            &mut |iterations| {
                for _ in 0..iterations {
                    each(&self.opensearch, theirs)?;
                    theirs += 1;
                }
                Ok(())
            },
        )
    }
}

fn metrics(name: &'static str, measured: measure::Paired) -> Vec<Metric> {
    vec![
        Metric::reported(name, "ratio_to_opensearch", measured.ratio),
        Metric::reported(name, "eggdb_ns_per_request", measured.subject_ns),
        Metric::reported(name, "opensearch_ns_per_request", measured.reference_ns),
    ]
}

pub(crate) fn bulk(servers: &Servers) -> Result<Vec<Metric>, String> {
    fresh_index(&servers.eggdb, BULK_INDEX)?;
    fresh_index(&servers.opensearch, BULK_INDEX)?;
    let measured = servers.paired(&|server, number| {
        server.bulk("/_bulk", &bulk_body(BULK_INDEX, number * BULK_SIZE, BULK_SIZE))
    })?;
    Ok(metrics(BULK, measured))
}

pub(crate) fn get(servers: &Servers) -> Result<Vec<Metric>, String> {
    servers.with_read_index()?;
    let measured = servers.paired(&|server, number| {
        server
            .call("GET", &format!("/{READ_INDEX}/_doc/{}", number % DOCUMENTS), "")
            .map(drop)
    })?;
    Ok(metrics(GET, measured))
}

pub(crate) fn search_match(servers: &Servers) -> Result<Vec<Metric>, String> {
    servers.with_read_index()?;
    let measured = servers.paired(&|server, number| {
        server
            .call(
                "POST",
                &format!("/{READ_INDEX}/_search"),
                &format!("{{\"size\":10,\"query\":{{\"match\":{{\"title\":\"{}\"}}}}}}", word(number)),
            )
            .map(drop)
    })?;
    Ok(metrics(MATCH, measured))
}

pub(crate) fn terms_aggregation(servers: &Servers) -> Result<Vec<Metric>, String> {
    servers.with_read_index()?;
    let measured = servers.paired(&|server, _| {
        server
            .call(
                "POST",
                &format!("/{READ_INDEX}/_search"),
                "{\"size\":0,\"aggs\":{\"by_category\":{\"terms\":{\"field\":\"category\"}}}}",
            )
            .map(drop)
    })?;
    Ok(metrics(TERMS, measured))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_bodies_pair_an_action_with_each_document() {
        let body = bulk_body("i", 17, 2);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "{\"index\":{\"_index\":\"i\",\"_id\":\"17\"}}");
        assert_eq!(lines[1], document(17));
    }
}
