use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::database::failed;
use crate::measure;
use eggdb::Database;
use eggdb::sql::{Dialect, Session, Value};
use std::hint::black_box;

pub(crate) const HYBRID: &str = "retrieval.hybrid_recall";

const DOCUMENTS: u64 = 2_000;
const WORDS_PER_DOCUMENT: u64 = 12;
const WORDS_PER_QUERY: u64 = 4;
const QUERIES: u64 = 200;
const RECALLED: u64 = 10;
const VOCABULARY: u64 = 400;
const AGENT: &str = "bench";

struct Words(u64);

impl Words {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

fn word(number: u64) -> String {
    let consonants = b"bcdfghklmnprstvz";
    let vowels = b"aeiou";
    let mut out = String::new();
    let mut value = number % VOCABULARY + VOCABULARY;
    while value > 0 {
        out.push(char::from(consonants[(value % 16) as usize]));
        value /= 16;
        out.push(char::from(vowels[(value % 5) as usize]));
        value /= 5;
    }
    out
}

fn execute(session: &mut Session<'_>, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let mut rows = Vec::new();
    session
        .execute(sql, &mut |result| rows = result.rows)
        .map_err(failed(sql))?;
    Ok(rows)
}

pub(crate) fn hybrid(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let db = Database::in_memory().map_err(failed("open"))?;
    let store = db.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    execute(&mut session, "CREATE EXTENSION eggdb_ai")?;
    let mut words = Words(7);
    let mut documents: Vec<(i64, Vec<String>)> = Vec::new();
    for _ in 0..DOCUMENTS {
        let text: Vec<String> = (0..WORDS_PER_DOCUMENT).map(|_| word(words.next())).collect();
        let rows = execute(
            &mut session,
            &format!("SELECT eggdb_ai.remember('{AGENT}', '{}')", text.join(" ")),
        )?;
        let id = rows.first().and_then(|row| row.first()).and_then(Value::as_integer).ok_or("remember gave no id")?;
        documents.push((id, text));
    }
    let queries: Vec<(i64, String)> = (0..QUERIES)
        .map(|at| {
            let (id, text) = &documents[((at * 7_919) % DOCUMENTS) as usize];
            let query: Vec<&str> = text.iter().take(WORDS_PER_QUERY as usize).map(String::as_str).collect();
            (*id, query.join(" "))
        })
        .collect();
    let mut found = 0u64;
    for (id, query) in &queries {
        let rows = execute(
            &mut session,
            &format!("SELECT id FROM eggdb_ai.recall('{AGENT}', '{query}', {RECALLED})"),
        )?;
        if rows.iter().any(|row| row.first().and_then(Value::as_integer) == Some(*id)) {
            found += 1;
        }
    }
    let mut next = 0usize;
    let per_query = measure::single(&mut |iterations| {
        for _ in 0..iterations {
            let (_, query) = &queries[next % queries.len()];
            next += 1;
            black_box(execute(
                &mut session,
                &format!("SELECT id FROM eggdb_ai.recall('{AGENT}', '{query}', {RECALLED})"),
            )?);
        }
        Ok(())
    })?;
    Ok(vec![
        Metric::reported(HYBRID, "recall_at_10", found as f64 / QUERIES as f64),
        Metric::reported(HYBRID, "ns_per_query", per_query),
    ])
}
