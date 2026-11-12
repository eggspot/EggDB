use crate::baseline::Metric;
use crate::client::MongoClient;
use crate::measure;
use crate::ycsb_mix::{
    Driver, Mix, Operation, RECORDS, WORKLOAD_A, WORKLOAD_B, WORKLOAD_C, WORKLOAD_D, WORKLOAD_E, WORKLOAD_F,
};
use eggdb_bson::{Bson, Document};
use std::hint::black_box;

pub(crate) const INSERT: &str = "mongo_compare.insert";
pub(crate) const FIND_RANGE: &str = "mongo_compare.find_range";
pub(crate) const GROUP: &str = "mongo_compare.group";
pub(crate) const YCSB: [(&str, Mix); 6] = [
    ("mongo_compare.ycsb_a", WORKLOAD_A),
    ("mongo_compare.ycsb_b", WORKLOAD_B),
    ("mongo_compare.ycsb_c", WORKLOAD_C),
    ("mongo_compare.ycsb_d", WORKLOAD_D),
    ("mongo_compare.ycsb_e", WORKLOAD_E),
    ("mongo_compare.ycsb_f", WORKLOAD_F),
];

const DATABASE: &str = "eggdb_compare";
const INSERTED: &str = "inserted";
const RECORDS_COLLECTION: &str = "records";
const BATCH: u64 = 100;
const FILL_BATCH: u64 = 1000;
const FIELD_LENGTH: usize = 100;

#[derive(Debug)]
pub(crate) struct Servers {
    pub(crate) eggdb: MongoClient,
    pub(crate) mongodb: MongoClient,
    pub(crate) records_loaded: bool,
}

fn entries(entries: Vec<(&str, Bson)>) -> Document {
    Document::from_entries(entries.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

fn int(value: u64) -> Bson {
    Bson::Int64(i64::try_from(value).unwrap_or(i64::MAX))
}

fn field_value(index: u64, version: u64) -> Bson {
    let digits = format!("{index:016x}{version:04x}");
    Bson::String(digits.repeat(FIELD_LENGTH / digits.len() + 1)[..FIELD_LENGTH].to_owned())
}

fn record(index: u64) -> Bson {
    Bson::Document(entries(vec![
        ("_id", int(index)),
        ("field0", field_value(index, 0)),
        ("category", Bson::String(format!("c{}", index % 20))),
        ("price", int(index % 1000)),
    ]))
}

fn insert(client: &mut MongoClient, collection: &str, first: u64, count: u64) -> Result<(), String> {
    let documents: Vec<Bson> = (first..first + count).map(record).collect();
    client
        .command(
            DATABASE,
            entries(vec![
                ("insert", Bson::String(collection.to_owned())),
                ("documents", Bson::Array(documents)),
                ("ordered", Bson::Boolean(false)),
            ]),
        )
        .map(drop)
}

fn dropped(client: &mut MongoClient, collection: &str) {
    let _ = client.command(DATABASE, entries(vec![("drop", Bson::String(collection.to_owned()))]));
}

fn first_batch_length(reply: &Document) -> usize {
    reply
        .get_document("cursor")
        .and_then(|cursor| cursor.get_array("firstBatch"))
        .map_or(0, Vec::len)
}

fn find(client: &mut MongoClient, filter: Document, sort: Option<Document>, limit: u64) -> Result<usize, String> {
    let mut body = vec![
        ("find", Bson::String(RECORDS_COLLECTION.to_owned())),
        ("filter", Bson::Document(filter)),
        ("limit", int(limit)),
        ("singleBatch", Bson::Boolean(true)),
    ];
    if let Some(sort) = sort {
        body.push(("sort", Bson::Document(sort)));
    }
    client.command(DATABASE, entries(body)).map(|reply| first_batch_length(&reply))
}

fn by_id(index: u64) -> Document {
    entries(vec![("_id", int(index))])
}

fn set_field(client: &mut MongoClient, index: u64, version: u64) -> Result<(), String> {
    let update = entries(vec![
        ("q", Bson::Document(by_id(index))),
        ("u", Bson::Document(entries(vec![("$set", Bson::Document(entries(vec![("field0", field_value(index, version))])))]))),
        ("upsert", Bson::Boolean(true)),
    ]);
    client
        .command(
            DATABASE,
            entries(vec![
                ("update", Bson::String(RECORDS_COLLECTION.to_owned())),
                ("updates", Bson::Array(vec![Bson::Document(update)])),
            ]),
        )
        .map(drop)
}

fn operations(client: &mut MongoClient, driver: &mut Driver, count: u64) -> Result<usize, String> {
    let mut folded = 0;
    for step in 0..count {
        match driver.next() {
            (Operation::Read, index) => folded ^= find(client, by_id(index), None, 1)?,
            (Operation::Scan(length), index) => {
                let from = entries(vec![("_id", Bson::Document(entries(vec![("$gte", int(index))])))]);
                folded ^= find(client, from, Some(entries(vec![("_id", Bson::Int32(1))])), length)?;
            }
            (Operation::Update, index) => set_field(client, index, step + 1)?,
            (Operation::Insert, index) => insert(client, RECORDS_COLLECTION, index, 1)?,
            (Operation::ReadModifyWrite, index) => {
                folded ^= find(client, by_id(index), None, 1)?;
                set_field(client, index, step + 1)?;
            }
        }
    }
    Ok(folded)
}

fn metrics(name: &'static str, measured: measure::Paired) -> Vec<Metric> {
    vec![
        Metric::reported(name, "ratio_to_mongodb", measured.ratio),
        Metric::reported(name, "eggdb_ns_per_operation", measured.subject_ns),
        Metric::reported(name, "mongodb_ns_per_operation", measured.reference_ns),
    ]
}

impl Servers {
    fn on_both(
        &mut self,
        each: &dyn Fn(&mut MongoClient, u64) -> Result<usize, String>,
    ) -> Result<measure::Paired, String> {
        let (mut ours, mut theirs) = (0u64, 0u64);
        let (eggdb, mongodb) = (&mut self.eggdb, &mut self.mongodb);
        measure::paired(
            &mut |iterations| {
                for _ in 0..iterations {
                    black_box(each(eggdb, ours)?);
                    ours += 1;
                }
                Ok(())
            },
            &mut |iterations| {
                for _ in 0..iterations {
                    black_box(each(mongodb, theirs)?);
                    theirs += 1;
                }
                Ok(())
            },
        )
    }

    fn with_records(&mut self) -> Result<(), String> {
        if self.records_loaded {
            return Ok(());
        }
        for client in [&mut self.eggdb, &mut self.mongodb] {
            dropped(client, RECORDS_COLLECTION);
            for first in (0..RECORDS).step_by(FILL_BATCH as usize) {
                insert(client, RECORDS_COLLECTION, first, FILL_BATCH.min(RECORDS - first))?;
            }
        }
        self.records_loaded = true;
        Ok(())
    }
}

pub(crate) fn inserts(servers: &mut Servers) -> Result<Vec<Metric>, String> {
    dropped(&mut servers.eggdb, INSERTED);
    dropped(&mut servers.mongodb, INSERTED);
    let measured = servers.on_both(&|client, number| insert(client, INSERTED, number * BATCH, BATCH).map(|()| 0))?;
    Ok(metrics(INSERT, measured))
}

pub(crate) fn find_range(servers: &mut Servers) -> Result<Vec<Metric>, String> {
    servers.with_records()?;
    let measured = servers.on_both(&|client, number| {
        let low = number % 990;
        let filter = entries(vec![(
            "price",
            Bson::Document(entries(vec![("$gte", int(low)), ("$lt", int(low + 10))])),
        )]);
        find(client, filter, None, 10)
    })?;
    Ok(metrics(FIND_RANGE, measured))
}

pub(crate) fn group(servers: &mut Servers) -> Result<Vec<Metric>, String> {
    servers.with_records()?;
    let measured = servers.on_both(&|client, _| {
        let pipeline = vec![Bson::Document(entries(vec![(
            "$group",
            Bson::Document(entries(vec![
                ("_id", Bson::String("$category".to_owned())),
                ("total", Bson::Document(entries(vec![("$sum", Bson::String("$price".to_owned()))]))),
            ])),
        )]))];
        client
            .command(
                DATABASE,
                entries(vec![
                    ("aggregate", Bson::String(RECORDS_COLLECTION.to_owned())),
                    ("pipeline", Bson::Array(pipeline)),
                    ("cursor", Bson::Document(Document::new())),
                ]),
            )
            .map(|reply| first_batch_length(&reply))
    })?;
    Ok(metrics(GROUP, measured))
}

pub(crate) fn ycsb(servers: &mut Servers, name: &'static str, mix: Mix) -> Result<Vec<Metric>, String> {
    servers.with_records()?;
    let mut ours = Driver::new(mix, 1);
    let mut theirs = Driver::new(mix, 1);
    let (eggdb, mongodb) = (&mut servers.eggdb, &mut servers.mongodb);
    let measured = measure::paired(
        &mut |count| {
            black_box(operations(eggdb, &mut ours, count)?);
            Ok(())
        },
        &mut |count| {
            black_box(operations(mongodb, &mut theirs, count)?);
            Ok(())
        },
    )?;
    servers.records_loaded = !matches!(name, "mongo_compare.ycsb_d" | "mongo_compare.ycsb_e");
    Ok(metrics(name, measured))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_carry_a_fixed_length_field() {
        let Bson::Document(record) = record(42) else {
            panic!("a record is a document");
        };
        assert_eq!(record.get_str("field0").map(str::len), Some(FIELD_LENGTH));
        assert_eq!(record.get_str("category"), Some("c2"));
    }
}
