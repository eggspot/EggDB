use super::{Answer, Api, error, json};
use crate::json_out::escaped;
use eggdb_http::Response;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const NODE: &str = "eggdb";
const MAX_KEPT_TASKS: usize = 1000;

struct FinishedTask {
    action: &'static str,
    description: String,
    running_nanos: u64,
    status: u16,
    report: String,
}

#[derive(Default)]
pub(super) struct Tasks {
    next: AtomicU64,
    finished: Mutex<BTreeMap<u64, FinishedTask>>,
}

impl std::fmt::Debug for Tasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tasks").finish_non_exhaustive()
    }
}

fn task_number(id: &str) -> Option<u64> {
    let (node, number) = id.split_once(':')?;
    if node != NODE {
        return None;
    }
    number.parse().ok()
}

fn missing_task(id: &str) -> Response {
    error(
        404,
        "resource_not_found_exception",
        &format!("task [{id}] isn't running and hasn't stored its results"),
    )
}

impl Api<'_> {
    pub(super) fn keep_finished_task(
        &self,
        action: &'static str,
        description: String,
        running_nanos: u64,
        response: &Response,
    ) -> Response {
        let number = self.tasks.next.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut finished) = self.tasks.finished.lock() {
            while finished.len() >= MAX_KEPT_TASKS {
                if finished.pop_first().is_none() {
                    break;
                }
            }
            finished.insert(
                number,
                FinishedTask {
                    action,
                    description,
                    running_nanos,
                    status: response.status,
                    report: String::from_utf8_lossy(&response.body).into_owned(),
                },
            );
        }
        json(200, format!("{{\"task\":\"{NODE}:{number}\"}}"))
    }

    pub(super) fn get_task(&self, id: &str) -> Answer {
        let Some(number) = task_number(id) else {
            return Ok(missing_task(id));
        };
        let Ok(finished) = self.tasks.finished.lock() else {
            return Ok(missing_task(id));
        };
        let Some(task) = finished.get(&number) else {
            return Ok(missing_task(id));
        };
        let outcome = if task.status < 300 { "response" } else { "error" };
        Ok(json(
            200,
            format!(
                "{{\"completed\":true,\"task\":{{\"node\":\"{NODE}\",\"id\":{number},\"type\":\"transport\",\"action\":\"{}\",\"status\":{},\"description\":{},\"start_time_in_millis\":0,\"running_time_in_nanos\":{},\"cancellable\":true,\"cancelled\":false,\"headers\":{{}}}},\"{outcome}\":{}}}",
                task.action,
                task.report,
                escaped(&task.description),
                task.running_nanos,
                task.report
            ),
        ))
    }

    pub(super) fn list_tasks(&self) -> Answer {
        Ok(json(200, "{\"nodes\":{}}".to_owned()))
    }

    pub(super) fn settle_task(&self, id: &str) -> Answer {
        if task_number(id).is_none() {
            return Ok(missing_task(id));
        }
        Ok(json(200, "{\"nodes\":{}}".to_owned()))
    }
}
