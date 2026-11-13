//! The Prometheus workloads: remote write (protobuf `WriteRequest` bodies, snappy-compressed)
//! to `/api/v1/write`, then PromQL range queries through `/api/v1/query_range`, the same
//! requests to EggDB's `--observability-listen` and to Prometheus with its remote-write
//! receiver on.

use crate::http::{Connection, Request, query_encoded};
use crate::measure::{Measured, timed};

/// The series and samples written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    pub(crate) series: usize,
    pub(crate) samples_per_series: usize,
    pub(crate) samples_per_request: usize,
    pub(crate) last_millis: i64,
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn length_delimited(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    varint(out, u64::from(field << 3 | 2));
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn label(name: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    length_delimited(&mut out, 1, name.as_bytes());
    length_delimited(&mut out, 2, value.as_bytes());
    out
}

fn sample(value: f64, millis: i64) -> Vec<u8> {
    let mut out = vec![0x09];
    out.extend_from_slice(&value.to_le_bytes());
    out.push(0x10);
    varint(&mut out, millis as u64);
    out
}

fn request_body(shape: Shape, request: usize) -> Vec<u8> {
    let per_series = (shape.samples_per_request / shape.series).max(1);
    let first = request * per_series;
    let last = (first + per_series).min(shape.samples_per_series);
    let mut write_request = Vec::new();
    for series in 0..shape.series {
        let mut timeseries = Vec::new();
        length_delimited(&mut timeseries, 1, &label("__name__", "bench_requests_total"));
        length_delimited(&mut timeseries, 1, &label("instance", &format!("host-{series}")));
        length_delimited(&mut timeseries, 1, &label("job", "bench"));
        for at in first..last {
            let millis = shape.last_millis - (shape.samples_per_series - 1 - at) as i64 * 1000;
            let value = (at * (series + 1)) as f64;
            length_delimited(&mut timeseries, 2, &sample(value, millis));
        }
        length_delimited(&mut write_request, 1, &timeseries);
    }
    eggdb_columnar::snappy_compress(&write_request)
}

/// How many remote-write requests the workload takes.
pub(crate) fn request_count(shape: Shape) -> usize {
    let per_series = (shape.samples_per_request / shape.series).max(1);
    shape.samples_per_series.div_ceil(per_series)
}

/// Writes every sample, each request carrying the next samples of every series, in time order.
pub(crate) fn write(address: &str, authorization: Option<String>, shape: Shape) -> Result<Measured, String> {
    let mut connection = Connection::open(address, authorization)?;
    let requests = request_count(shape);
    let mut measured = timed(requests, 0, &mut |request| {
        let body = request_body(shape, request);
        let (status, answer) = connection.send(&Request {
            method: "POST",
            path: "/api/v1/write",
            headers: &[
                ("content-type", "application/x-protobuf"),
                ("content-encoding", "snappy"),
                ("x-prometheus-remote-write-version", "0.1.0"),
            ],
            body: &body,
        })?;
        if !(200..300).contains(&status) {
            return Err(format!("a remote write answered {status}: {}", String::from_utf8_lossy(&answer)));
        }
        Ok(())
    })?;
    measured.units = (shape.series * shape.samples_per_series) as u64;
    Ok(measured)
}

/// Runs `runs` PromQL range queries over the written range, each a per-instance rate summed by
/// job, and checks that each answers a matrix.
pub(crate) fn queries(address: &str, authorization: Option<String>, shape: Shape, runs: usize) -> Result<Measured, String> {
    let mut connection = Connection::open(address, authorization)?;
    let end = shape.last_millis / 1000;
    let start = end - shape.samples_per_series as i64 + 60;
    let query = query_encoded("sum by (job) (rate(bench_requests_total[1m]))");
    let path = format!("/api/v1/query_range?query={query}&start={start}&end={end}&step=15");
    timed(runs, 1, &mut |_| {
        let (status, answer) = connection.send(&Request { method: "GET", path: &path, headers: &[], body: &[] })?;
        let text = String::from_utf8_lossy(&answer);
        if status != 200 || !text.contains("\"resultType\":\"matrix\"") || text.contains("\"result\":[]") {
            return Err(format!("a range query answered {status}: {text}"));
        }
        Ok(())
    })
}
