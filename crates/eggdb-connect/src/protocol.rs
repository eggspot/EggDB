//! Kafka Connect's worker group protocol (D1055): the metadata a worker joins the group with
//! (its REST URL and the configuration offset it has read) and the assignment the leader sends
//! back (the connectors and tasks each worker runs), in the layout Connect's documentation gives
//! for its "default" protocol, and the leader's round-robin assignment.

use std::collections::BTreeMap;

/// The task number Connect uses for a connector's own instance in an assignment.
pub(crate) const CONNECTOR_TASK: i32 = -1;

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len().min(i16::MAX as usize) as i16).to_be_bytes());
    out.extend_from_slice(&text.as_bytes()[..text.len().min(i16::MAX as usize)]);
}

struct Reader<'b> {
    bytes: &'b [u8],
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Option<&'b [u8]> {
        let (taken, rest) = self.bytes.split_at_checked(count)?;
        self.bytes = rest;
        Some(taken)
    }

    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let length = usize::try_from(self.i16()?).ok()?;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }

    fn count(&mut self, smallest: usize) -> Option<usize> {
        let count = usize::try_from(self.i32()?).ok()?;
        (count <= self.bytes.len() / smallest.max(1)).then_some(count)
    }
}

/// A worker's join metadata: version 0, its URL and its configuration offset.
pub(crate) fn encode_metadata(url: &str, config_offset: i64) -> Vec<u8> {
    let mut out = 0i16.to_be_bytes().to_vec();
    put_string(&mut out, url);
    out.extend_from_slice(&config_offset.to_be_bytes());
    out
}

/// Reads [`encode_metadata`]'s bytes into the URL and configuration offset.
pub(crate) fn decode_metadata(bytes: &[u8]) -> Option<(String, i64)> {
    let mut reader = Reader { bytes };
    reader.i16()?;
    Some((reader.string()?, reader.i64()?))
}

/// One worker's assignment as the leader sends it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Assignment {
    pub(crate) error: i16,
    pub(crate) leader: String,
    pub(crate) leader_url: String,
    pub(crate) config_offset: i64,
    pub(crate) work: BTreeMap<String, Vec<i32>>,
}

pub(crate) fn encode_assignment(assignment: &Assignment) -> Vec<u8> {
    let mut out = 0i16.to_be_bytes().to_vec();
    out.extend_from_slice(&assignment.error.to_be_bytes());
    put_string(&mut out, &assignment.leader);
    put_string(&mut out, &assignment.leader_url);
    out.extend_from_slice(&assignment.config_offset.to_be_bytes());
    out.extend_from_slice(&(assignment.work.len() as i32).to_be_bytes());
    for (connector, tasks) in &assignment.work {
        put_string(&mut out, connector);
        out.extend_from_slice(&(tasks.len() as i32).to_be_bytes());
        for task in tasks {
            out.extend_from_slice(&task.to_be_bytes());
        }
    }
    out
}

pub(crate) fn decode_assignment(bytes: &[u8]) -> Option<Assignment> {
    let mut reader = Reader { bytes };
    reader.i16()?;
    let error = reader.i16()?;
    let leader = reader.string()?;
    let leader_url = reader.string()?;
    let config_offset = reader.i64()?;
    let mut work = BTreeMap::new();
    for _ in 0..reader.count(6)? {
        let connector = reader.string()?;
        let tasks = (0..reader.count(4)?).map(|_| reader.i32()).collect::<Option<Vec<i32>>>()?;
        work.insert(connector, tasks);
    }
    Some(Assignment { error, leader, leader_url, config_offset, work })
}

/// The leader's round robin: every connector instance, then every task, dealt to the workers
/// in member-id order, so each worker runs about the same number of each.
pub(crate) fn assign(workers: &[String], connectors: &BTreeMap<String, i32>) -> BTreeMap<String, BTreeMap<String, Vec<i32>>> {
    let mut sorted: Vec<&String> = workers.iter().collect();
    sorted.sort();
    let mut out: BTreeMap<String, BTreeMap<String, Vec<i32>>> = sorted.iter().map(|worker| ((*worker).clone(), BTreeMap::new())).collect();
    if sorted.is_empty() {
        return out;
    }
    let mut next = 0usize;
    let mut deal = |connector: &str, task: i32, out: &mut BTreeMap<String, BTreeMap<String, Vec<i32>>>| {
        let worker = sorted[next % sorted.len()].clone();
        next += 1;
        out.entry(worker).or_default().entry(connector.to_owned()).or_default().push(task);
    };
    for connector in connectors.keys() {
        deal(connector, CONNECTOR_TASK, &mut out);
    }
    for (connector, tasks) in connectors {
        for task in 0..(*tasks).max(0) {
            deal(connector, task, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_and_assignments_round_trip_and_spread_evenly() {
        assert_eq!(decode_metadata(&encode_metadata("http://w1:8083/", 42)), Some(("http://w1:8083/".to_owned(), 42)));
        let connectors: BTreeMap<String, i32> = [("files".to_owned(), 3), ("copy".to_owned(), 1)].into_iter().collect();
        let plan = assign(&["w2".to_owned(), "w1".to_owned()], &connectors);
        let counts: Vec<usize> = plan.values().map(|work| work.values().map(Vec::len).sum()).collect();
        assert_eq!(counts.iter().sum::<usize>(), 6);
        assert!(counts.iter().all(|count| *count == 3));
        let assignment = Assignment {
            error: 0,
            leader: "w1".to_owned(),
            leader_url: "http://w1:8083/".to_owned(),
            config_offset: 7,
            work: plan["w1"].clone(),
        };
        assert_eq!(decode_assignment(&encode_assignment(&assignment)), Some(assignment));
        assert_eq!(decode_assignment(&[0, 0, 0]), None);
        let mut huge = encode_assignment(&Assignment::default());
        let at = huge.len() - 4;
        huge[at..].copy_from_slice(&i32::MAX.to_be_bytes());
        assert_eq!(decode_assignment(&huge), None);
    }
}
