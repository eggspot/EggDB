//! EggDB's fleet control plane (D58, D1112): software operators run themselves to manage
//! thousands of EggDB servers. Every server sends a heartbeat with its version, cluster,
//! region and hardware to one control-plane server (`eggdb serve --fleet-listen`), which keeps
//! the inventory in its own database, plans upgrades in canary waves and gates each wave on the
//! last one's health ([`plan`]), tells each server the version it should run, and chooses
//! servers for new databases by hardware class.

pub mod plan;

use eggdb_http::{Request, Response};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use eggdb_sql::json::{Json, Node, parse_document};
use plan::{Hardware, Member, Step, UpgradePlan, next_step, place, plan_upgrade};
use std::collections::BTreeMap;
use std::time::Duration;

const SPACE: u32 = 0;
const MEMBER: u8 = b'm';
const ON_TARGET: u8 = b't';
const UPGRADE: &[u8] = b"upgrade";
const MAX_MEMBERS: usize = 100_000;

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn member_json(member: &Member, now: u64) -> String {
    format!(
        "{{\"id\":{},\"cluster\":{},\"region\":{},\"version\":{},\"class\":{},\"healthy\":{},\"up\":{},\"last_seen_ms\":{},\"hardware\":{{\"arch\":{},\"cpus\":{},\"memory_gib\":{},\"storage\":{}}}}}",
        json_string(&member.id),
        json_string(&member.cluster),
        json_string(&member.region),
        json_string(&member.version),
        json_string(&member.hardware_class()),
        member.healthy,
        member.up(now),
        member.last_seen_ms,
        json_string(&member.hardware.arch),
        member.hardware.cpus,
        member.hardware.memory_gib,
        json_string(&member.hardware.storage),
    )
}

fn field<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Node> {
    members.iter().find(|(member, _)| member == name).map(|(_, node)| node)
}

fn text(node: Option<&Node>) -> Option<String> {
    match &node?.value {
        Json::String(text) => Some(text.clone()),
        _ => None,
    }
}

fn number(node: Option<&Node>) -> Option<u64> {
    match &node?.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

fn read_member(body: &str) -> Option<Member> {
    let Json::Object(fields) = parse_document(body).ok()?.value else {
        return None;
    };
    let hardware = match field(&fields, "hardware").map(|node| &node.value) {
        Some(Json::Object(parts)) => Hardware {
            arch: text(field(parts, "arch")).unwrap_or_default(),
            cpus: u32::try_from(number(field(parts, "cpus")).unwrap_or(0)).unwrap_or(u32::MAX),
            memory_gib: u32::try_from(number(field(parts, "memory_gib")).unwrap_or(0)).unwrap_or(u32::MAX),
            storage: text(field(parts, "storage")).unwrap_or_default(),
        },
        _ => Hardware::default(),
    };
    let id = text(field(&fields, "id")).filter(|id| !id.is_empty() && id.len() <= 256)?;
    Some(Member {
        cluster: text(field(&fields, "cluster")).unwrap_or_else(|| id.clone()),
        region: text(field(&fields, "region")).unwrap_or_default(),
        version: text(field(&fields, "version")).unwrap_or_default(),
        class: text(field(&fields, "class")).filter(|class| !class.is_empty()),
        healthy: !matches!(field(&fields, "healthy").map(|node| &node.value), Some(Json::Boolean(false))),
        last_seen_ms: number(field(&fields, "last_seen_ms")).unwrap_or(0),
        hardware,
        id,
    })
}

fn plan_json(plan: &UpgradePlan) -> String {
    let waves: Vec<String> = plan
        .waves
        .iter()
        .map(|wave| format!("[{}]", wave.iter().map(|id| json_string(id)).collect::<Vec<_>>().join(",")))
        .collect();
    format!("{{\"target\":{},\"soak_ms\":{},\"waves\":[{}]}}", json_string(&plan.target), plan.soak_ms, waves.join(","))
}

fn read_plan(body: &str) -> Option<UpgradePlan> {
    let Json::Object(fields) = parse_document(body).ok()?.value else {
        return None;
    };
    let waves = match field(&fields, "waves").map(|node| &node.value) {
        Some(Json::Array(waves)) => waves
            .iter()
            .map(|wave| match &wave.value {
                Json::Array(ids) => ids.iter().map(|id| text(Some(id))).collect::<Option<Vec<_>>>(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    Some(UpgradePlan { target: text(field(&fields, "target"))?, waves, soak_ms: number(field(&fields, "soak_ms")).unwrap_or(0) })
}

fn step_json(step: &Step) -> String {
    match step {
        Step::Finished => "{\"step\":\"finished\"}".to_owned(),
        Step::Run(wave) => format!("{{\"step\":\"run\",\"wave\":{wave}}}"),
        Step::Wait { wave, reason } => format!("{{\"step\":\"wait\",\"wave\":{wave},\"reason\":{}}}", json_string(reason)),
        Step::Halt(ids) => format!("{{\"step\":\"halt\",\"roll_back\":[{}]}}", ids.iter().map(|id| json_string(id)).collect::<Vec<_>>().join(",")),
    }
}

fn reply(status: u16, body: String) -> Response {
    let mut response = Response::ok("application/json", body.into_bytes());
    response.status = status;
    response
}

fn refused(status: u16, reason: &str) -> Response {
    reply(status, format!("{{\"error\":{}}}", json_string(reason)))
}

/// The control plane's state, kept in a keyspace of the control-plane server's own database.
pub struct Fleet<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

impl std::fmt::Debug for Fleet<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fleet")
    }
}

impl<'k> Fleet<'k> {
    /// The fleet kept in `keyspace`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    fn read<T>(&self, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace.read(SPACE, &mut |keys| {
            if let Some(body) = body.take() {
                out = Some(body(keys)?);
            }
            Ok(())
        })?;
        out.ok_or_else(|| "the read did not run".to_owned())
    }

    fn write<T>(&self, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace.write(SPACE, &mut |keys| {
            if let Some(body) = body.take() {
                out = Some(body(keys)?);
            }
            Ok(())
        })?;
        out.ok_or_else(|| "the write did not run".to_owned())
    }

    /// Every server in the inventory.
    pub fn members(&self) -> Result<Vec<Member>, String> {
        self.read(|keys| {
            Ok(keys
                .scan(&[MEMBER])?
                .into_iter()
                .filter_map(|(_, value)| read_member(&String::from_utf8_lossy(&value)))
                .collect())
        })
    }

    fn on_target_since(&self) -> Result<BTreeMap<String, u64>, String> {
        self.read(|keys| {
            Ok(keys
                .scan(&[ON_TARGET])?
                .into_iter()
                .filter_map(|(stored, value)| {
                    Some((String::from_utf8_lossy(&stored[1..]).into_owned(), u64::from_be_bytes(value.try_into().ok()?)))
                })
                .collect())
        })
    }

    /// The upgrade in progress, if any.
    pub fn upgrade(&self) -> Result<Option<UpgradePlan>, String> {
        self.read(|keys| Ok(keys.get(UPGRADE)?.and_then(|bytes| read_plan(&String::from_utf8_lossy(&bytes)))))
    }

    /// Records `member`'s heartbeat at `now`, and when it first ran the upgrade's target.
    pub fn heartbeat(&self, mut member: Member, now: u64) -> Result<(), String> {
        member.last_seen_ms = now;
        let target = self.upgrade()?.map(|plan| plan.target);
        self.write(|keys| {
            let mut stored = vec![MEMBER];
            stored.extend_from_slice(member.id.as_bytes());
            if keys.get(&stored)?.is_none() && keys.scan(&[MEMBER])?.len() >= MAX_MEMBERS {
                return Err(format!("the fleet holds {MAX_MEMBERS} servers at most"));
            }
            keys.put(&stored, member_json(&member, now).as_bytes())?;
            let mut since = vec![ON_TARGET];
            since.extend_from_slice(member.id.as_bytes());
            if target.as_deref() == Some(member.version.as_str()) {
                if keys.get(&since)?.is_none() {
                    keys.put(&since, &now.to_be_bytes())?;
                }
            } else {
                keys.delete(&since)?;
            }
            Ok(())
        })
    }

    /// Starts an upgrade of every server to `target` in waves.
    pub fn start_upgrade(&self, target: &str, fractions: &[f64], soak_ms: u64) -> Result<UpgradePlan, String> {
        let plan = plan_upgrade(&self.members()?, target, fractions, soak_ms);
        let encoded = plan_json(&plan);
        self.write(|keys| {
            for (stored, _) in keys.scan(&[ON_TARGET])? {
                keys.delete(&stored)?;
            }
            keys.put(UPGRADE, encoded.as_bytes())
        })?;
        Ok(plan)
    }

    /// Ends the upgrade in progress.
    pub fn cancel_upgrade(&self) -> Result<bool, String> {
        self.write(|keys| keys.delete(UPGRADE))
    }

    /// The upgrade's next step at `now`.
    pub fn step(&self, now: u64) -> Result<Option<(UpgradePlan, Step)>, String> {
        let Some(plan) = self.upgrade()? else {
            return Ok(None);
        };
        let step = next_step(&plan, &self.members()?, &self.on_target_since()?, now);
        Ok(Some((plan, step)))
    }

    /// The version server `id` should run now: the upgrade's target once its wave may run,
    /// the version it runs otherwise, and nothing for a server that must roll back (its
    /// operator returns it to the version before the upgrade).
    pub fn desired_version(&self, id: &str, running: &str, now: u64) -> Result<Option<String>, String> {
        let Some((plan, step)) = self.step(now)? else {
            return Ok(Some(running.to_owned()));
        };
        let wave = plan.waves.iter().position(|wave| wave.iter().any(|member| member == id));
        Ok(match (&step, wave) {
            (Step::Halt(back), _) if back.iter().any(|member| member == id) => None,
            (Step::Run(current) | Step::Wait { wave: current, .. }, Some(wave)) if wave <= *current => Some(plan.target.clone()),
            (Step::Finished, Some(_)) => Some(plan.target.clone()),
            _ => Some(running.to_owned()),
        })
    }
}

/// The control plane's HTTP API.
pub struct FleetEndpoint<'k> {
    fleet: Fleet<'k>,
    password: &'k str,
}

impl std::fmt::Debug for FleetEndpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FleetEndpoint")
    }
}

impl<'k> FleetEndpoint<'k> {
    /// The API over the fleet kept in `keyspace`, for callers giving `password` as a bearer
    /// token.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), password: &'k str) -> Self {
        Self { fleet: Fleet::new(keyspace), password }
    }

    /// Answers one request: `POST /fleet/heartbeat`, `GET /fleet/inventory`,
    /// `POST`/`GET`/`DELETE /fleet/upgrades` and `POST /fleet/placements`.
    pub fn answer(&self, request: &Request) -> Response {
        let given = request.header("authorization").and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("");
        if !eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(given.trim().as_bytes()), &eggdb_crypto::sha256(self.password.as_bytes())) {
            return refused(401, "send the control plane's password as Authorization: Bearer <password>").with_header("WWW-Authenticate", "Bearer");
        }
        let body = String::from_utf8_lossy(&request.body);
        let now = now_ms();
        let outcome = match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/fleet/heartbeat") => {
                let Some(member) = read_member(&body) else {
                    return refused(400, "a heartbeat is a JSON object with at least an id");
                };
                let (id, running) = (member.id.clone(), member.version.clone());
                self.fleet.heartbeat(member, now).and_then(|()| self.fleet.desired_version(&id, &running, now)).map(|desired| {
                    reply(200, format!("{{\"desired_version\":{}}}", desired.map_or_else(|| "null".to_owned(), |version| json_string(&version))))
                })
            }
            ("GET", "/fleet/inventory") => self.fleet.members().map(|members| {
                reply(200, format!("{{\"members\":[{}]}}", members.iter().map(|member| member_json(member, now)).collect::<Vec<_>>().join(",")))
            }),
            ("POST", "/fleet/upgrades") => {
                let Some(Json::Object(fields)) = parse_document(&body).ok().map(|node| node.value) else {
                    return refused(400, "an upgrade is a JSON object with a target");
                };
                let Some(target) = text(field(&fields, "target")) else {
                    return refused(400, "an upgrade needs a target version");
                };
                let fractions: Vec<f64> = match field(&fields, "fractions").map(|node| &node.value) {
                    Some(Json::Array(items)) => items.iter().filter_map(|item| match &item.value {
                        Json::Number(text) => text.parse().ok(),
                        _ => None,
                    }).collect(),
                    _ => vec![0.1, 0.5],
                };
                let soak_ms = number(field(&fields, "soak_seconds")).unwrap_or(600).saturating_mul(1_000);
                self.fleet.start_upgrade(&target, &fractions, soak_ms).map(|plan| reply(201, plan_json(&plan)))
            }
            ("GET", "/fleet/upgrades") => self.fleet.step(now).map(|found| match found {
                Some((plan, step)) => reply(200, format!("{{\"plan\":{},\"next\":{}}}", plan_json(&plan), step_json(&step))),
                None => refused(404, "no upgrade is in progress"),
            }),
            ("DELETE", "/fleet/upgrades") => self.fleet.cancel_upgrade().map(|_| reply(200, "{\"ok\":true}".to_owned())),
            ("POST", "/fleet/placements") => {
                let Some(Json::Object(fields)) = parse_document(&body).ok().map(|node| node.value) else {
                    return refused(400, "a placement is a JSON object with a class");
                };
                let Some(class) = text(field(&fields, "class")) else {
                    return refused(400, "a placement needs a hardware class");
                };
                let region = text(field(&fields, "region"));
                let replicas = usize::try_from(number(field(&fields, "replicas")).unwrap_or(3)).unwrap_or(3).clamp(1, 16);
                self.fleet.members().map(|members| {
                    let chosen = place(&members, &class, region.as_deref(), replicas, &BTreeMap::new(), now);
                    if chosen.len() < replicas {
                        return refused(409, &format!("only {} up servers of class {class} are available", chosen.len()));
                    }
                    reply(200, format!("{{\"members\":[{}]}}", chosen.iter().map(|id| json_string(id)).collect::<Vec<_>>().join(",")))
                })
            }
            _ => return refused(404, "the fleet API is /fleet/heartbeat, /fleet/inventory, /fleet/upgrades and /fleet/placements"),
        };
        outcome.unwrap_or_else(|message| refused(500, &message))
    }
}

/// Sends one heartbeat for `member` to the control plane at `url` with `password`; returns the
/// version the control plane wants this server to run (`None` for a roll-back).
pub fn report(url: &str, password: &str, member: &Member, timeout: Duration) -> Result<Option<String>, String> {
    let target = eggdb_http::client::parse_http_url(url)?;
    let body = member_json(member, member.last_seen_ms);
    let headers = vec![
        ("Authorization".to_owned(), format!("Bearer {password}")),
        ("Content-Type".to_owned(), "application/json".to_owned()),
    ];
    let path = format!("{}/fleet/heartbeat", target.path.trim_end_matches('/'));
    let answer = eggdb_http::client::request(&target, "POST", &path, &headers, body.as_bytes(), timeout, 1 << 20)
        .map_err(|error| format!("cannot reach the control plane at {url}: {error}"))?;
    if answer.status != 200 {
        return Err(format!("the control plane answered {}: {}", answer.status, String::from_utf8_lossy(&answer.body).trim()));
    }
    let text_body = String::from_utf8_lossy(&answer.body);
    let Some(Json::Object(fields)) = parse_document(&text_body).ok().map(|node| node.value) else {
        return Err("the control plane's answer is not JSON".to_owned());
    };
    Ok(text(field(&fields, "desired_version")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn heartbeat(id: &str, cluster: &str, region: &str, version: &str) -> String {
        format!(
            "{{\"id\":\"{id}\",\"cluster\":\"{cluster}\",\"region\":\"{region}\",\"version\":\"{version}\",\"healthy\":true,\"hardware\":{{\"arch\":\"aarch64\",\"cpus\":16,\"memory_gib\":64,\"storage\":\"nvme\"}}}}"
        )
    }

    fn call(endpoint: &FleetEndpoint<'_>, method: &str, path: &str, body: &str) -> Response {
        endpoint.answer(&Request {
            method: method.to_owned(),
            path: path.to_owned(),
            query: None,
            headers: vec![("authorization".to_owned(), "Bearer secret".to_owned())],
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn heartbeats_fill_the_inventory_and_an_upgrade_reaches_its_canary_first() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = FleetEndpoint::new(&keyspace, "secret");
        for (id, cluster, region) in [("a1", "a", "eu"), ("a2", "a", "us"), ("b1", "b", "eu"), ("b2", "b", "us")] {
            let answer = call(&endpoint, "POST", "/fleet/heartbeat", &heartbeat(id, cluster, region, "0.5.0"));
            assert_eq!(String::from_utf8_lossy(&answer.body), "{\"desired_version\":\"0.5.0\"}");
        }
        let inventory = String::from_utf8_lossy(&call(&endpoint, "GET", "/fleet/inventory", "").body).into_owned();
        assert_eq!(inventory.matches("\"class\":\"aarch64-medium-nvme\"").count(), 4);
        let started = call(&endpoint, "POST", "/fleet/upgrades", "{\"target\":\"0.6.0\",\"fractions\":[0.5],\"soak_seconds\":0}");
        assert_eq!(started.status, 201);
        let plan = read_plan(&String::from_utf8_lossy(&started.body)).unwrap();
        let canary = plan.waves[0][0].clone();
        let canary_answer = call(&endpoint, "POST", "/fleet/heartbeat", &heartbeat(&canary, &canary[..1], "eu", "0.5.0"));
        assert!(String::from_utf8_lossy(&canary_answer.body).contains("0.6.0"));
        let later = plan.waves.last().unwrap()[0].clone();
        if !plan.waves[0].contains(&later) {
            let later_answer = call(&endpoint, "POST", "/fleet/heartbeat", &heartbeat(&later, &later[..1], "us", "0.5.0"));
            assert!(String::from_utf8_lossy(&later_answer.body).contains("0.5.0"));
        }
        let placed = call(&endpoint, "POST", "/fleet/placements", "{\"class\":\"aarch64-medium-nvme\",\"replicas\":2}");
        assert_eq!(placed.status, 200, "{}", String::from_utf8_lossy(&placed.body));
        assert_eq!(call(&endpoint, "POST", "/fleet/placements", "{\"class\":\"x86_64-large-hdd\",\"replicas\":2}").status, 409);
    }

    #[test]
    fn the_control_plane_wants_its_password() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = FleetEndpoint::new(&keyspace, "secret");
        let answer = endpoint.answer(&Request { method: "GET".to_owned(), path: "/fleet/inventory".to_owned(), query: None, headers: Vec::new(), body: Vec::new() });
        assert_eq!(answer.status, 401);
        assert_eq!(call(&endpoint, "POST", "/fleet/heartbeat", "{\"cluster\":\"x\"}").status, 400);
    }
}
