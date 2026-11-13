//! The InfluxDB workloads: line protocol written through `/api/v2/write` in batches, then Flux
//! queries through `/api/v2/query`, the same requests to EggDB's `--observability-listen` and to
//! InfluxDB 2.

use crate::http::{Connection, Request, query_encoded};
use crate::measure::{Measured, timed};

/// Where one server takes the workload.
pub(crate) struct Target<'t> {
    pub(crate) address: &'t str,
    pub(crate) token: &'t str,
    pub(crate) org: &'t str,
    pub(crate) bucket: &'t str,
}

/// The series and points written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    pub(crate) hosts: usize,
    pub(crate) points_per_host: usize,
    pub(crate) lines_per_batch: usize,
    pub(crate) last_second: i64,
}

fn batch_body(shape: Shape, batch: usize) -> String {
    let mut out = String::with_capacity(shape.lines_per_batch * 80);
    let first = batch * shape.lines_per_batch;
    let total = shape.hosts * shape.points_per_host;
    for line in first..(first + shape.lines_per_batch).min(total) {
        let point = line / shape.hosts;
        let host = line % shape.hosts;
        let second = shape.last_second - (shape.points_per_host - 1 - point) as i64;
        let usage = ((line * 7919) % 1000) as f64 / 10.0;
        out.push_str(&format!(
            "bench,host=host-{host},region=r{} usage={usage},requests={}i {second}\n",
            host % 4,
            line % 977
        ));
    }
    out
}

fn connect(target: &Target<'_>) -> Result<Connection, String> {
    Connection::open(target.address, Some(format!("Token {}", target.token)))
}

/// Writes every point, one batch per request, with second precision.
pub(crate) fn write(target: &Target<'_>, shape: Shape) -> Result<Measured, String> {
    let mut connection = connect(target)?;
    let total = shape.hosts * shape.points_per_host;
    let batches = total.div_ceil(shape.lines_per_batch);
    let path = format!("/api/v2/write?org={}&bucket={}&precision=s", query_encoded(target.org), query_encoded(target.bucket));
    let mut measured = timed(batches, shape.lines_per_batch as u64, &mut |batch| {
        let body = batch_body(shape, batch);
        let (status, answer) = connection.send(&Request {
            method: "POST",
            path: &path,
            headers: &[("content-type", "text/plain; charset=utf-8")],
            body: body.as_bytes(),
        })?;
        if status != 204 && status != 200 {
            return Err(format!("a write answered {status}: {}", String::from_utf8_lossy(&answer)));
        }
        Ok(())
    })?;
    measured.units = total as u64;
    Ok(measured)
}

/// Runs `runs` Flux queries, each a one-host window mean over the written range, and checks that
/// each answers rows.
pub(crate) fn queries(target: &Target<'_>, shape: Shape, runs: usize) -> Result<Measured, String> {
    let mut connection = connect(target)?;
    let start = shape.last_second - shape.points_per_host as i64 - 60;
    let stop = shape.last_second + 60;
    let path = format!("/api/v2/query?org={}", query_encoded(target.org));
    timed(runs, 1, &mut |run| {
        let host = (run * 31) % shape.hosts;
        let flux = format!(
            "from(bucket: \"{}\") |> range(start: {start}, stop: {stop}) |> filter(fn: (r) => r._measurement == \"bench\" and r._field == \"usage\" and r.host == \"host-{host}\") |> aggregateWindow(every: 1m, fn: mean)",
            target.bucket
        );
        let (status, answer) = connection.send(&Request {
            method: "POST",
            path: &path,
            headers: &[("content-type", "application/vnd.flux"), ("accept", "application/csv")],
            body: flux.as_bytes(),
        })?;
        if status != 200 {
            return Err(format!("a query answered {status}: {}", String::from_utf8_lossy(&answer)));
        }
        if !String::from_utf8_lossy(&answer).contains("_value") {
            return Err(format!("a query answered no rows: {}", String::from_utf8_lossy(&answer)));
        }
        Ok(())
    })
}
