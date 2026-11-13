//! The fleet's decisions, as pure functions of its inventory (D1112): which hardware class a
//! server is, which servers an upgrade reaches in each wave, whether a wave may go ahead, and
//! where new replicas go.

use std::collections::{BTreeMap, BTreeSet};

/// A server's hardware, as it reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hardware {
    /// The CPU architecture, such as `x86_64` or `aarch64`.
    pub arch: String,
    /// The logical CPUs.
    pub cpus: u32,
    /// The memory, in GiB.
    pub memory_gib: u32,
    /// The storage kind: `nvme`, `ssd`, `hdd` or `network`.
    pub storage: String,
}

/// One server of the fleet, as its last heartbeat described it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Member {
    /// The server's id, unique in the fleet.
    pub id: String,
    /// The cluster it belongs to (its replication group), or its own id when alone.
    pub cluster: String,
    /// Its region.
    pub region: String,
    /// The EggDB version it runs.
    pub version: String,
    /// Its hardware.
    pub hardware: Hardware,
    /// A class its operator gave it, which wins over the one its hardware implies.
    pub class: Option<String>,
    /// Whether its last heartbeat said it was serving.
    pub healthy: bool,
    /// When its last heartbeat came, in milliseconds since 1970.
    pub last_seen_ms: u64,
}

/// How long a server may stay silent before it counts as down: 90 seconds, three heartbeats.
pub const SILENT_AFTER_MS: u64 = 90_000;

impl Member {
    /// Its hardware class: the one its operator gave, or `<arch>-<size>-<storage>` with the size
    /// `small` (under 8 CPUs or 32 GiB), `medium` (under 32 CPUs or 128 GiB) or `large`.
    pub fn hardware_class(&self) -> String {
        if let Some(class) = &self.class {
            return class.clone();
        }
        let size = if self.hardware.cpus < 8 || self.hardware.memory_gib < 32 {
            "small"
        } else if self.hardware.cpus < 32 || self.hardware.memory_gib < 128 {
            "medium"
        } else {
            "large"
        };
        let storage = if self.hardware.storage.is_empty() { "ssd" } else { &self.hardware.storage };
        format!("{}-{size}-{storage}", if self.hardware.arch.is_empty() { "unknown" } else { &self.hardware.arch })
    }

    /// Whether it is serving and was heard from within [`SILENT_AFTER_MS`] of `now_ms`.
    pub fn up(&self, now_ms: u64) -> bool {
        self.healthy && now_ms.saturating_sub(self.last_seen_ms) <= SILENT_AFTER_MS
    }
}

/// An upgrade of the fleet to one version, in waves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    /// The version every server moves to.
    pub target: String,
    /// The servers of each wave, in order; the first wave is the canaries.
    pub waves: Vec<Vec<String>>,
    /// How long each wave runs on the new version, all healthy, before the next starts.
    pub soak_ms: u64,
}

/// Plans an upgrade of every server not yet on `target`: the first wave is one canary per
/// hardware class and region, so every kind of machine meets the new version early; the
/// following waves take the rest by `fractions` of the fleet (such as 0.1, 0.5 and 1.0). No wave
/// holds two servers of one cluster, so a replication group never loses more than one member to
/// an upgrade at a time.
pub fn plan_upgrade(members: &[Member], target: &str, fractions: &[f64], soak_ms: u64) -> UpgradePlan {
    let mut pending: Vec<&Member> = members.iter().filter(|member| member.version != target).collect();
    pending.sort_by(|a, b| (a.hardware_class(), &a.region, &a.cluster, &a.id).cmp(&(b.hardware_class(), &b.region, &b.cluster, &b.id)));
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut canaries = Vec::new();
    let mut seen_kinds = BTreeSet::new();
    let mut canary_clusters = BTreeSet::new();
    for member in &pending {
        let kind = (member.hardware_class(), member.region.clone());
        if !seen_kinds.contains(&kind) && !canary_clusters.contains(&member.cluster) {
            seen_kinds.insert(kind);
            canary_clusters.insert(member.cluster.clone());
            canaries.push(member.id.clone());
        }
    }
    pending.retain(|member| !canaries.contains(&member.id));
    if !canaries.is_empty() {
        waves.push(canaries);
    }
    let total = members.len().max(1) as f64;
    let mut done = waves.first().map_or(0, Vec::len);
    for fraction in fractions.iter().chain(std::iter::once(&1.0)) {
        let reach = ((fraction.clamp(0.0, 1.0) * total).ceil() as usize).max(done + 1);
        while done < reach && !pending.is_empty() {
            let mut wave = Vec::new();
            let mut clusters = BTreeSet::new();
            let mut left = Vec::new();
            for member in pending.drain(..) {
                if done + wave.len() < reach && clusters.insert(member.cluster.clone()) {
                    wave.push(member.id.clone());
                } else {
                    left.push(member);
                }
            }
            pending = left;
            if wave.is_empty() {
                break;
            }
            done += wave.len();
            waves.push(wave);
        }
    }
    UpgradePlan { target: target.to_owned(), waves, soak_ms }
}

/// What an upgrade does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Every wave is done.
    Finished,
    /// Wave `n` (from 0) may run: its servers should move to the target.
    Run(usize),
    /// Wait: wave `wave` is running and not yet on the target, healthy, for the whole soak.
    Wait {
        /// The running wave.
        wave: usize,
        /// What it waits for.
        reason: String,
    },
    /// Stop and roll the named servers back: they took the target and are down.
    Halt(Vec<String>),
}

/// Decides an upgrade's next step from the inventory at `now_ms`, given when each server
/// reached the target (`on_target_since`).
pub fn next_step(plan: &UpgradePlan, members: &[Member], on_target_since: &BTreeMap<String, u64>, now_ms: u64) -> Step {
    let by_id: BTreeMap<&str, &Member> = members.iter().map(|member| (member.id.as_str(), member)).collect();
    for (number, wave) in plan.waves.iter().enumerate() {
        let failed: Vec<String> = wave
            .iter()
            .filter(|id| by_id.get(id.as_str()).is_some_and(|member| member.version == plan.target && !member.up(now_ms)))
            .cloned()
            .collect();
        if !failed.is_empty() {
            return Step::Halt(failed);
        }
        let started = wave.iter().any(|id| by_id.get(id.as_str()).is_some_and(|member| member.version == plan.target));
        if !started {
            return Step::Run(number);
        }
        for id in wave {
            let Some(member) = by_id.get(id.as_str()) else {
                continue;
            };
            if member.version != plan.target {
                return Step::Wait { wave: number, reason: format!("wave {number}: {id} is still on {}", member.version) };
            }
            let since = on_target_since.get(id).copied().unwrap_or(now_ms);
            if now_ms.saturating_sub(since) < plan.soak_ms {
                return Step::Wait {
                    wave: number,
                    reason: format!("wave {number}: {id} has run the new version for {} of {} ms", now_ms.saturating_sub(since), plan.soak_ms),
                };
            }
        }
    }
    Step::Finished
}

/// Picks `replicas` servers of hardware class `class` for a new database, one per region while
/// regions last (and within `region` when given), preferring servers that hold fewer
/// placements (`load`), so replicas spread over failure domains and machines fill evenly.
pub fn place(members: &[Member], class: &str, region: Option<&str>, replicas: usize, load: &BTreeMap<String, u64>, now_ms: u64) -> Vec<String> {
    let mut candidates: Vec<&Member> = members
        .iter()
        .filter(|member| member.up(now_ms) && member.hardware_class() == class && region.is_none_or(|region| member.region == region))
        .collect();
    candidates.sort_by_key(|member| (load.get(&member.id).copied().unwrap_or(0), member.id.clone()));
    let mut chosen: Vec<String> = Vec::new();
    let mut regions = BTreeSet::new();
    for member in &candidates {
        if chosen.len() < replicas && regions.insert(member.region.clone()) {
            chosen.push(member.id.clone());
        }
    }
    for member in &candidates {
        if chosen.len() < replicas && !chosen.contains(&member.id) {
            chosen.push(member.id.clone());
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, cluster: &str, region: &str, cpus: u32, version: &str) -> Member {
        Member {
            id: id.to_owned(),
            cluster: cluster.to_owned(),
            region: region.to_owned(),
            version: version.to_owned(),
            hardware: Hardware { arch: "x86_64".to_owned(), cpus, memory_gib: cpus * 4, storage: "nvme".to_owned() },
            class: None,
            healthy: true,
            last_seen_ms: 1_000,
        }
    }

    fn fleet() -> Vec<Member> {
        let mut members = Vec::new();
        for cluster in 0..10 {
            for replica in 0..3 {
                let region = ["eu", "us", "ap"][replica];
                let cpus = if cluster < 5 { 4 } else { 16 };
                members.push(member(&format!("c{cluster}-{replica}"), &format!("c{cluster}"), region, cpus, "0.5.0"));
            }
        }
        members
    }

    #[test]
    fn canaries_cover_every_class_and_region_and_no_wave_takes_two_of_a_cluster() {
        let members = fleet();
        let plan = plan_upgrade(&members, "0.6.0", &[0.1, 0.5], 600_000);
        let kinds: BTreeSet<(String, String)> = plan.waves[0]
            .iter()
            .map(|id| members.iter().find(|member| &member.id == id).unwrap())
            .map(|member| (member.hardware_class(), member.region.clone()))
            .collect();
        assert_eq!(kinds.len(), plan.waves[0].len());
        assert!(plan.waves[0].len() >= 2);
        for wave in &plan.waves {
            let clusters: BTreeSet<&str> = wave.iter().map(|id| id.split('-').next().unwrap()).collect();
            assert_eq!(clusters.len(), wave.len(), "{wave:?}");
        }
        let everyone: usize = plan.waves.iter().map(Vec::len).sum();
        assert_eq!(everyone, 30);
    }

    #[test]
    fn a_wave_waits_for_its_soak_and_a_failed_canary_halts_the_upgrade() {
        let mut members = fleet();
        let plan = plan_upgrade(&members, "0.6.0", &[0.5], 1_000);
        assert_eq!(next_step(&plan, &members, &BTreeMap::new(), 1_000), Step::Run(0));
        let mut since = BTreeMap::new();
        for id in &plan.waves[0] {
            let member = members.iter_mut().find(|member| &member.id == id).unwrap();
            member.version = "0.6.0".to_owned();
            since.insert(id.clone(), 1_000);
        }
        assert!(matches!(next_step(&plan, &members, &since, 1_500), Step::Wait { wave: 0, .. }));
        assert_eq!(next_step(&plan, &members, &since, 2_000), Step::Run(1));
        let canary = plan.waves[0][0].clone();
        members.iter_mut().find(|member| member.id == canary).unwrap().healthy = false;
        assert_eq!(next_step(&plan, &members, &since, 2_000), Step::Halt(vec![canary]));
    }

    #[test]
    fn placement_spreads_replicas_over_regions_within_a_class() {
        let members = fleet();
        let class = members[15].hardware_class();
        let chosen = place(&members, &class, None, 3, &BTreeMap::new(), 1_000);
        let regions: BTreeSet<&str> = chosen.iter().map(|id| members.iter().find(|member| &member.id == id).unwrap().region.as_str()).collect();
        assert_eq!((chosen.len(), regions.len()), (3, 3));
        assert!(chosen.iter().all(|id| members.iter().find(|member| &member.id == id).unwrap().hardware_class() == class));
        assert!(place(&members, "arm-large-hdd", None, 3, &BTreeMap::new(), 1_000).is_empty());
        let silent = place(&members, &class, None, 3, &BTreeMap::new(), 1_000 + SILENT_AFTER_MS + 1);
        assert!(silent.is_empty());
    }
}
