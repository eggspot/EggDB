//! The workloads both servers run, each timed the same way: prepared single-row inserts,
//! unlogged batches of inserts, and prepared point reads, with a window of requests in flight
//! on one connection.

use crate::client::{Client, is_result};
use std::time::{Duration, Instant};

/// What one workload measured on one server.
#[derive(Debug, Clone, Default)]
pub(crate) struct Measured {
    pub(crate) operations: u64,
    pub(crate) elapsed: Duration,
    pub(crate) latencies_us: Vec<u64>,
}

impl Measured {
    pub(crate) fn per_second(&self) -> f64 {
        self.operations as f64 / self.elapsed.as_secs_f64().max(1e-9)
    }

    pub(crate) fn percentile(&self, percent: f64) -> u64 {
        if self.latencies_us.is_empty() {
            return 0;
        }
        let mut sorted = self.latencies_us.clone();
        sorted.sort_unstable();
        sorted[(((sorted.len() - 1) as f64) * percent / 100.0).round() as usize]
    }
}

const KEYSPACE: &str = "eggdb_compare";
const WINDOW: usize = 64;
const BATCH_ROWS: usize = 50;

fn int(value: i32) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

fn text(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn value_for(id: i32) -> String {
    format!("{id:08}-{}", "v".repeat(90))
}

/// Drops and creates the comparison's keyspace and table.
pub(crate) fn prepare_schema(client: &mut Client) -> Result<(), String> {
    client.query(&format!("DROP KEYSPACE IF EXISTS {KEYSPACE}"))?;
    client.query(&format!(
        "CREATE KEYSPACE {KEYSPACE} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
    ))?;
    client.query(&format!("CREATE TABLE {KEYSPACE}.kv (id int PRIMARY KEY, value text)"))?;
    Ok(())
}

fn windowed(client: &mut Client, count: usize, send: &mut dyn FnMut(&mut Client, i16, usize) -> Result<(), String>) -> Result<Measured, String> {
    let mut latencies_us = Vec::new();
    let started = Instant::now();
    let mut done = 0;
    while done < count {
        let window_started = Instant::now();
        let in_window = WINDOW.min(count - done);
        for slot in 0..in_window {
            send(client, (slot + 1) as i16, done + slot)?;
        }
        for _ in 0..in_window {
            let answer = client.receive()?;
            answer.check()?;
            if !is_result(answer.opcode) || answer.stream < 1 || answer.stream as usize > in_window {
                return Err(format!("an answer on stream {} that was not asked for", answer.stream));
            }
        }
        done += in_window;
        latencies_us.push(window_started.elapsed().as_micros() as u64);
    }
    Ok(Measured { operations: count as u64, elapsed: started.elapsed(), latencies_us })
}

/// Inserts rows `0..rows` one prepared `INSERT` each.
pub(crate) fn inserts(client: &mut Client, rows: usize) -> Result<Measured, String> {
    let id = client.prepare(&format!("INSERT INTO {KEYSPACE}.kv (id, value) VALUES (?, ?)"))?;
    windowed(client, rows, &mut |client, stream, row| {
        client.execute(stream, &id, &[int(row as i32), text(&value_for(row as i32))])
    })
}

/// Inserts rows `start..start + rows` in unlogged batches of 50.
pub(crate) fn batches(client: &mut Client, start: usize, rows: usize) -> Result<Measured, String> {
    let id = client.prepare(&format!("INSERT INTO {KEYSPACE}.kv (id, value) VALUES (?, ?)"))?;
    let batch_count = rows.div_ceil(BATCH_ROWS);
    let mut measured = windowed(client, batch_count, &mut |client, stream, batch| {
        let first = start + batch * BATCH_ROWS;
        let last = (first + BATCH_ROWS).min(start + rows);
        let bound: Vec<Vec<Vec<u8>>> = (first..last).map(|row| vec![int(row as i32), text(&value_for(row as i32))]).collect();
        client.batch(stream, &id, &bound)
    })?;
    measured.operations = rows as u64;
    Ok(measured)
}

/// Reads `reads` rows by key, spread over `0..rows`, one prepared `SELECT` each.
pub(crate) fn point_reads(client: &mut Client, rows: usize, reads: usize) -> Result<Measured, String> {
    let id = client.prepare(&format!("SELECT value FROM {KEYSPACE}.kv WHERE id = ?"))?;
    windowed(client, reads, &mut |client, stream, read| {
        let key = (read * 7919) % rows.max(1);
        client.execute(stream, &id, &[int(key as i32)])
    })
}

/// Counts the rows the table holds through a full scan, as a check that both servers hold the
/// same data before they are compared.
pub(crate) fn counted_rows(client: &mut Client) -> Result<u64, String> {
    client.query(&format!("SELECT id FROM {KEYSPACE}.kv"))?.row_count()
}
