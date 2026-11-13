use crate::baseline::Metric;
use crate::bolt::{BoltClient, Value};
use crate::measure;
use crate::social::{self, DATE_MIDDLE, Scale, mixed};
use std::hint::black_box;

pub(crate) const PERSON_PROFILE: &str = "ldbc_compare.is1_person_profile";
pub(crate) const RECENT_POSTS: &str = "ldbc_compare.is2_recent_posts";
pub(crate) const FRIENDS: &str = "ldbc_compare.is3_friends";
pub(crate) const FRIENDS_BY_NAME: &str = "ldbc_compare.ic1_friends_by_name";
pub(crate) const FRIENDS_POSTS: &str = "ldbc_compare.ic2_friends_recent_posts";
pub(crate) const SHORTEST_PATH: &str = "ldbc_compare.ic13_shortest_path";
pub(crate) const ADD_FRIENDSHIP: &str = "ldbc_compare.iu8_add_friendship";
pub(crate) const LOAD: &str = "ldbc_compare.load";

const BATCH: u64 = 500;

#[derive(Debug)]
pub(crate) struct Servers {
    pub(crate) eggdb: BoltClient,
    pub(crate) neo4j: BoltClient,
    pub(crate) scale: Scale,
    pub(crate) loaded: bool,
}

fn int(value: u64) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn rows(name: &str, rows: Vec<Value>) -> Vec<(String, Value)> {
    vec![(name.to_owned(), Value::List(rows))]
}

fn load(client: &mut BoltClient, scale: Scale) -> Result<(), String> {
    client.run("MATCH (n) DETACH DELETE n", Vec::new())?;
    let _ = client.run("CREATE INDEX ldbc_person_id IF NOT EXISTS FOR (p:Person) ON (p.id)", Vec::new());
    let _ = client.run("CREATE INDEX ldbc_post_id IF NOT EXISTS FOR (m:Post) ON (m.id)", Vec::new());
    let mut first = 0;
    while first < scale.persons {
        let end = (first + BATCH).min(scale.persons);
        client.run(
            "UNWIND $rows AS r CREATE (:Person {id: r.id, firstName: r.firstName, lastName: r.lastName, birthday: r.birthday, creationDate: r.creationDate})",
            rows("rows", social::persons(first, end)),
        )?;
        first = end;
    }
    let mut pending = Vec::new();
    let mut posts = Vec::new();
    let mut likes = Vec::new();
    for person in 0..scale.persons {
        pending.extend(social::friendships(person, scale));
        posts.extend(social::posts(person, scale));
        likes.extend(social::likes(person, scale));
        if pending.len() as u64 >= BATCH || person + 1 == scale.persons {
            client.run(
                "UNWIND $rows AS r MATCH (a:Person {id: r.a}), (b:Person {id: r.b}) CREATE (a)-[:KNOWS {creationDate: r.creationDate}]->(b)",
                rows("rows", std::mem::take(&mut pending)),
            )?;
        }
        if posts.len() as u64 >= BATCH || person + 1 == scale.persons {
            client.run(
                "UNWIND $rows AS r MATCH (p:Person {id: r.creator}) CREATE (m:Post {id: r.id, content: r.content, length: r.length, creationDate: r.creationDate})-[:HAS_CREATOR]->(p)",
                rows("rows", std::mem::take(&mut posts)),
            )?;
        }
        if likes.len() as u64 >= BATCH || person + 1 == scale.persons {
            client.run(
                "UNWIND $rows AS r MATCH (p:Person {id: r.person}), (m:Post {id: r.post}) CREATE (p)-[:LIKES {creationDate: r.creationDate}]->(m)",
                rows("rows", std::mem::take(&mut likes)),
            )?;
        }
    }
    Ok(())
}

fn person(scale: Scale, step: u64) -> u64 {
    mixed(step ^ 0x5EED) % scale.persons.max(1)
}

fn read(client: &mut BoltClient, query: &str, parameters: Vec<(String, Value)>) -> Result<usize, String> {
    client.run(query, parameters).map(|records| records.len())
}

fn metrics(name: &'static str, measured: measure::Paired) -> Vec<Metric> {
    vec![
        Metric::reported(name, "ratio_to_neo4j", measured.ratio),
        Metric::reported(name, "eggdb_ns_per_operation", measured.subject_ns),
        Metric::reported(name, "neo4j_ns_per_operation", measured.reference_ns),
    ]
}

type Operation = fn(&mut BoltClient, Scale, u64) -> Result<usize, String>;

fn person_profile(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    read(
        client,
        "MATCH (n:Person {id: $id}) RETURN n.firstName, n.lastName, n.birthday, n.creationDate",
        vec![("id".into(), int(person(scale, step)))],
    )
}

fn recent_posts(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    read(
        client,
        "MATCH (:Person {id: $id})<-[:HAS_CREATOR]-(m:Post) RETURN m.id, m.content, m.creationDate ORDER BY m.creationDate DESC, m.id LIMIT 10",
        vec![("id".into(), int(person(scale, step)))],
    )
}

fn friends(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    read(
        client,
        "MATCH (n:Person {id: $id})-[r:KNOWS]-(f:Person) RETURN f.id, f.firstName, f.lastName, r.creationDate ORDER BY r.creationDate DESC, f.id",
        vec![("id".into(), int(person(scale, step)))],
    )
}

fn friends_by_name(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    let id = person(scale, step);
    read(
        client,
        "MATCH (p:Person {id: $id})-[:KNOWS*1..3]-(f:Person) WHERE f.firstName = $name AND f.id <> $id RETURN DISTINCT f.id, f.lastName ORDER BY f.lastName, f.id LIMIT 20",
        vec![("id".into(), int(id)), ("name".into(), Value::Text(social::first_name(id + 1).to_owned()))],
    )
}

fn friends_posts(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    read(
        client,
        "MATCH (:Person {id: $id})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Post) WHERE m.creationDate < $date RETURN f.id, m.id, m.creationDate ORDER BY m.creationDate DESC, m.id LIMIT 20",
        vec![("id".into(), int(person(scale, step))), ("date".into(), Value::Int(DATE_MIDDLE))],
    )
}

fn shortest_path(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    read(
        client,
        "MATCH (a:Person {id: $a}), (b:Person {id: $b}) MATCH path = shortestPath((a)-[:KNOWS*..15]-(b)) RETURN length(path)",
        vec![("a".into(), int(person(scale, step))), ("b".into(), int(person(scale, step ^ 0xABCD)))],
    )
}

fn add_friendship(client: &mut BoltClient, scale: Scale, step: u64) -> Result<usize, String> {
    read(
        client,
        "MATCH (a:Person {id: $a}), (b:Person {id: $b}) CREATE (a)-[:KNOWS {creationDate: $date}]->(b)",
        vec![
            ("a".into(), int(person(scale, step ^ 0x1111))),
            ("b".into(), int(person(scale, step ^ 0x2222))),
            ("date".into(), Value::Int(DATE_MIDDLE)),
        ],
    )
}


impl Servers {
    fn loaded(&mut self) -> Result<(), String> {
        if !self.loaded {
            load(&mut self.eggdb, self.scale)?;
            load(&mut self.neo4j, self.scale)?;
            self.loaded = true;
        }
        Ok(())
    }

    fn on_both(&mut self, each: Operation) -> Result<measure::Paired, String> {
        self.loaded()?;
        let scale = self.scale;
        let (mut ours, mut theirs) = (0u64, 0u64);
        let (eggdb, neo4j) = (&mut self.eggdb, &mut self.neo4j);
        measure::paired(
            &mut |iterations| {
                for _ in 0..iterations {
                    black_box(each(eggdb, scale, ours)?);
                    ours += 1;
                }
                Ok(())
            },
            &mut |iterations| {
                for _ in 0..iterations {
                    black_box(each(neo4j, scale, theirs)?);
                    theirs += 1;
                }
                Ok(())
            },
        )
    }

    pub(crate) fn measure(&mut self, name: &'static str) -> Result<Vec<Metric>, String> {
        let operation: Operation = match name {
            PERSON_PROFILE => person_profile,
            RECENT_POSTS => recent_posts,
            FRIENDS => friends,
            FRIENDS_BY_NAME => friends_by_name,
            FRIENDS_POSTS => friends_posts,
            SHORTEST_PATH => shortest_path,
            ADD_FRIENDSHIP => add_friendship,
            _ => return Err(format!("{name} is not an LDBC workload")),
        };
        self.on_both(operation).map(|measured| metrics(name, measured))
    }

    pub(crate) fn measure_load(&mut self) -> Result<Vec<Metric>, String> {
        let scale = self.scale;
        let started = std::time::Instant::now();
        load(&mut self.eggdb, scale)?;
        let ours = started.elapsed().as_secs_f64();
        let started = std::time::Instant::now();
        load(&mut self.neo4j, scale)?;
        let theirs = started.elapsed().as_secs_f64();
        self.loaded = true;
        Ok(vec![
            Metric::reported(LOAD, "ratio_to_neo4j", ours / theirs.max(f64::MIN_POSITIVE)),
            Metric::reported(LOAD, "eggdb_seconds", ours),
            Metric::reported(LOAD, "neo4j_seconds", theirs),
        ])
    }
}

pub(crate) const READS: [&str; 7] =
    [PERSON_PROFILE, RECENT_POSTS, FRIENDS, FRIENDS_BY_NAME, FRIENDS_POSTS, SHORTEST_PATH, ADD_FRIENDSHIP];
