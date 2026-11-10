use crate::{
    DocumentError, Modification, SortSpec, Store, Update, UpdateOptions, apply_update, matches,
    parse_filter, parse_projection, parse_sort, parse_update, parse_update_statement,
    sort_documents,
};
use eggdb_bson::{Bson, Document};
use eggdb_keyspace::MemoryKeyspace;

fn text(value: &str) -> Bson {
    Bson::String(value.to_owned())
}

fn doc(entries: Vec<(&str, Bson)>) -> Document {
    Document::from_entries(
        entries
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn person() -> Document {
    doc(vec![
        ("_id", Bson::Int32(1)),
        ("name", text("Ada")),
        ("age", Bson::Int32(36)),
        ("tags", Bson::Array(vec![text("math"), text("code")])),
        (
            "address",
            Bson::Document(doc(vec![("city", text("London")), ("zip", text("N1"))])),
        ),
        (
            "scores",
            Bson::Array(vec![
                Bson::Document(doc(vec![
                    ("subject", text("math")),
                    ("score", Bson::Int32(95)),
                ])),
                Bson::Document(doc(vec![
                    ("subject", text("art")),
                    ("score", Bson::Int32(60)),
                ])),
            ]),
        ),
    ])
}

fn holds(filter: Document) -> bool {
    matches(&parse_filter(&filter).unwrap(), &person())
}

#[test]
fn equality_reaches_into_documents_and_arrays() {
    assert!(holds(doc(vec![("name", text("Ada"))])));
    assert!(holds(doc(vec![("address.city", text("London"))])));
    assert!(holds(doc(vec![("tags", text("code"))])));
    assert!(holds(doc(vec![("tags.1", text("code"))])));
    assert!(holds(doc(vec![("scores.subject", text("art"))])));
    assert!(holds(doc(vec![("age", Bson::Double(36.0))])));
    assert!(holds(doc(vec![("missing", Bson::Null)])));
    assert!(!holds(doc(vec![("name", text("Grace"))])));
}

#[test]
fn comparison_and_logical_operators_hold_as_documented() {
    let op = |name: &str, value: Bson| Bson::Document(doc(vec![(name, value)]));
    assert!(holds(doc(vec![("age", op("$gt", Bson::Int32(30)))])));
    assert!(!holds(doc(vec![("age", op("$lt", Bson::Int32(30)))])));
    assert!(!holds(doc(vec![("age", op("$gt", text("a")))])));
    assert!(holds(doc(vec![(
        "age",
        op("$in", Bson::Array(vec![Bson::Int32(1), Bson::Int64(36)]))
    )])));
    assert!(holds(doc(vec![(
        "tags",
        op("$nin", Bson::Array(vec![text("art")]))
    )])));
    assert!(holds(doc(vec![(
        "tags",
        op("$all", Bson::Array(vec![text("code"), text("math")]))
    )])));
    assert!(holds(doc(vec![("tags", op("$size", Bson::Int32(2)))])));
    assert!(holds(doc(vec![(
        "missing",
        op("$exists", Bson::Boolean(false))
    )])));
    assert!(holds(doc(vec![("age", op("$type", text("number")))])));
    assert!(holds(doc(vec![(
        "age",
        op("$mod", Bson::Array(vec![Bson::Int32(5), Bson::Int32(1)]))
    )])));
    assert!(holds(doc(vec![("name", op("$regex", text("^a")))]).tap(
        |filter| {
            if let Some(Bson::Document(inner)) = filter.get_mut("name") {
                inner.insert("$options", text("i"));
            }
        }
    )));
    assert!(holds(doc(vec![(
        "age",
        op("$not", Bson::Document(doc(vec![("$lt", Bson::Int32(18))])))
    )])));
    assert!(holds(doc(vec![(
        "scores",
        op(
            "$elemMatch",
            Bson::Document(doc(vec![
                ("subject", text("math")),
                ("score", op("$gte", Bson::Int32(90)))
            ]))
        ),
    )])));
    assert!(!holds(doc(vec![(
        "scores",
        op(
            "$elemMatch",
            Bson::Document(doc(vec![
                ("subject", text("art")),
                ("score", op("$gte", Bson::Int32(90)))
            ]))
        ),
    )])));
    assert!(holds(doc(vec![(
        "$or",
        Bson::Array(vec![
            Bson::Document(doc(vec![("name", text("x"))])),
            Bson::Document(doc(vec![("age", Bson::Int32(36))]))
        ]),
    )])));
    assert!(!holds(doc(vec![(
        "$nor",
        Bson::Array(vec![Bson::Document(doc(vec![("age", Bson::Int32(36))]))])
    )])));
}

trait Tap: Sized {
    fn tap(self, change: impl FnOnce(&mut Self)) -> Self;
}

impl Tap for Document {
    fn tap(mut self, change: impl FnOnce(&mut Self)) -> Self {
        change(&mut self);
        self
    }
}

#[test]
fn bad_filters_are_errors() {
    let op = |name: &str, value: Bson| Bson::Document(doc(vec![(name, value)]));
    assert_eq!(
        parse_filter(&doc(vec![("$bogus", Bson::Int32(1))]))
            .unwrap_err()
            .code,
        2
    );
    assert_eq!(
        parse_filter(&doc(vec![("a", op("$bogus", Bson::Int32(1)))]))
            .unwrap_err()
            .code,
        2
    );
    assert!(parse_filter(&doc(vec![("$or", Bson::Array(Vec::new()))])).is_err());
    assert!(
        parse_filter(&doc(vec![(
            "a",
            op("$mod", Bson::Array(vec![Bson::Int32(0), Bson::Int32(1)]))
        )]))
        .is_err()
    );
    let mut deep = doc(vec![("a", Bson::Int32(1))]);
    for _ in 0..200 {
        deep = doc(vec![("$and", Bson::Array(vec![Bson::Document(deep)]))]);
    }
    assert!(parse_filter(&deep).is_err());
}

fn updated(update: Document) -> Document {
    let mut document = person();
    apply_update(
        &parse_update(&update).unwrap(),
        &mut document,
        None,
        false,
        1_000,
    )
    .unwrap();
    document
}

#[test]
fn update_operators_change_documents() {
    fn op<'a>(name: &'a str, fields: Vec<(&str, Bson)>) -> (&'a str, Bson) {
        (name, Bson::Document(doc(fields)))
    }
    let changed = updated(doc(vec![
        op(
            "$set",
            vec![
                ("address.city", text("Paris")),
                ("new.deep", Bson::Boolean(true)),
            ],
        ),
        op(
            "$inc",
            vec![("age", Bson::Int32(1)), ("visits", Bson::Int64(2))],
        ),
        op("$unset", vec![("address.zip", text(""))]),
        op("$push", vec![("tags", text("poetry"))]),
        op("$currentDate", vec![("seen", Bson::Boolean(true))]),
    ]));
    assert_eq!(
        changed.get_document("address").unwrap().get_str("city"),
        Some("Paris")
    );
    assert!(!changed.get_document("address").unwrap().contains_key("zip"));
    assert_eq!(changed.get("age"), Some(&Bson::Int32(37)));
    assert_eq!(changed.get("visits"), Some(&Bson::Int64(2)));
    assert_eq!(changed.get_array("tags").unwrap().len(), 3);
    assert_eq!(changed.get("seen"), Some(&Bson::DateTime(1_000)));
    assert_eq!(
        changed.get_document("new").unwrap().get("deep"),
        Some(&Bson::Boolean(true))
    );

    let changed = updated(doc(vec![
        op(
            "$addToSet",
            vec![(
                "tags",
                Bson::Document(doc(vec![(
                    "$each",
                    Bson::Array(vec![text("math"), text("art")]),
                )])),
            )],
        ),
        op(
            "$pull",
            vec![(
                "scores",
                Bson::Document(doc(vec![(
                    "score",
                    Bson::Document(doc(vec![("$lt", Bson::Int32(70))])),
                )])),
            )],
        ),
        op("$rename", vec![("name", text("fullName"))]),
        op("$max", vec![("age", Bson::Int32(40))]),
    ]));
    assert_eq!(
        changed.get_array("tags").unwrap(),
        &vec![text("math"), text("code"), text("art")]
    );
    assert_eq!(changed.get_array("scores").unwrap().len(), 1);
    assert_eq!(changed.get_str("fullName"), Some("Ada"));
    assert!(!changed.contains_key("name"));
    assert_eq!(changed.get("age"), Some(&Bson::Int32(40)));
}

#[test]
fn bad_updates_are_errors_and_change_nothing() {
    fn op<'a>(name: &'a str, fields: Vec<(&str, Bson)>) -> (&'a str, Bson) {
        (name, Bson::Document(doc(fields)))
    }
    assert_eq!(
        parse_update(&doc(vec![op("$bogus", vec![("a", Bson::Int32(1))])]))
            .unwrap_err()
            .code,
        9
    );
    assert_eq!(
        parse_update(&doc(vec![
            op("$set", vec![("a", Bson::Int32(1))]),
            op("$inc", vec![("a", Bson::Int32(1))])
        ]))
        .unwrap_err()
        .code,
        40
    );
    let mut document = person();
    let inc_text = parse_update(&doc(vec![
        op("$set", vec![("x", Bson::Int32(1))]),
        op("$inc", vec![("name", Bson::Int32(1))]),
    ]))
    .unwrap();
    assert_eq!(
        apply_update(&inc_text, &mut document, None, false, 0)
            .unwrap_err()
            .code,
        14
    );
    assert_eq!(document, person());
    let change_id = parse_update(&doc(vec![op("$set", vec![("_id", Bson::Int32(2))])])).unwrap();
    assert_eq!(
        apply_update(&change_id, &mut document, None, false, 0)
            .unwrap_err()
            .code,
        66
    );
    let overflow =
        parse_update(&doc(vec![op("$inc", vec![("age", Bson::Int64(i64::MAX))])])).unwrap();
    let mut big = doc(vec![("age", Bson::Int64(2))]);
    assert!(apply_update(&overflow, &mut big, None, false, 0).is_err());
}

#[test]
fn a_replacement_keeps_the_id() {
    let replacement = parse_update(&doc(vec![("name", text("Grace"))])).unwrap();
    assert!(matches!(replacement, Update::Replacement(_)));
    let mut document = person();
    apply_update(&replacement, &mut document, None, false, 0).unwrap();
    assert_eq!(
        document,
        doc(vec![("_id", Bson::Int32(1)), ("name", text("Grace"))])
    );
}

#[test]
fn projections_keep_or_drop_paths() {
    let keep = parse_projection(&doc(vec![
        ("name", Bson::Int32(1)),
        ("address.city", Bson::Int32(1)),
    ]))
    .unwrap();
    assert_eq!(
        keep.apply(&person()),
        doc(vec![
            ("_id", Bson::Int32(1)),
            ("name", text("Ada")),
            (
                "address",
                Bson::Document(doc(vec![("city", text("London"))]))
            )
        ])
    );
    let drop = parse_projection(&doc(vec![
        ("scores", Bson::Int32(0)),
        ("_id", Bson::Int32(0)),
        ("tags", Bson::Int32(0)),
    ]))
    .unwrap();
    assert_eq!(
        drop.apply(&person()).keys().cloned().collect::<Vec<_>>(),
        vec!["name", "age", "address"]
    );
    let only_id = parse_projection(&doc(vec![("_id", Bson::Int32(1))])).unwrap();
    assert_eq!(only_id.apply(&person()), doc(vec![("_id", Bson::Int32(1))]));
    assert!(parse_projection(&doc(vec![("a", Bson::Int32(1)), ("b", Bson::Int32(0))])).is_err());
}

#[test]
fn sorting_uses_the_type_order_and_array_bounds() {
    let mut documents = vec![
        doc(vec![("v", text("b"))]),
        doc(vec![("v", Bson::Int32(3))]),
        doc(vec![]),
        doc(vec![(
            "v",
            Bson::Array(vec![Bson::Int32(10), Bson::Int32(0)]),
        )]),
        doc(vec![("v", Bson::Double(1.5))]),
    ];
    sort_documents(
        &mut documents,
        &parse_sort(&doc(vec![("v", Bson::Int32(1))])).unwrap(),
    );
    let firsts: Vec<Option<&Bson>> = documents.iter().map(|document| document.get("v")).collect();
    assert_eq!(firsts[0], None);
    assert_eq!(
        firsts[1],
        Some(&Bson::Array(vec![Bson::Int32(10), Bson::Int32(0)]))
    );
    assert_eq!(firsts[2], Some(&Bson::Double(1.5)));
    assert_eq!(firsts[4], Some(&text("b")));
    assert!(parse_sort(&doc(vec![("v", Bson::Int32(2))])).is_err());
}

fn store(keyspace: &MemoryKeyspace) -> Store<'_> {
    Store::new(keyspace, [1, 2, 3, 4, 5, 6, 7, 8])
}

#[test]
fn a_store_inserts_finds_updates_and_deletes() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let outcome = store
        .insert(
            "app",
            "people",
            vec![
                person(),
                doc(vec![("name", text("Grace")), ("age", Bson::Int32(45))]),
            ],
            true,
        )
        .unwrap();
    assert_eq!(outcome.inserted, 2);
    let duplicate = store
        .insert(
            "app",
            "people",
            vec![doc(vec![("_id", Bson::Double(1.0))])],
            true,
        )
        .unwrap();
    assert_eq!(duplicate.errors[0].1.code, 11000);
    let everyone = store
        .find(
            "app",
            "people",
            &parse_filter(&Document::new()).unwrap(),
            &SortSpec::default(),
        )
        .unwrap();
    assert_eq!(everyone.len(), 2);
    assert!(matches!(everyone[1].first(), Some((name, Bson::ObjectId(_))) if name == "_id"));
    let older = parse_filter(&doc(vec![(
        "age",
        Bson::Document(doc(vec![("$gt", Bson::Int32(40))])),
    )]))
    .unwrap();
    let raise = parse_update(&doc(vec![(
        "$inc",
        Bson::Document(doc(vec![("age", Bson::Int32(1))])),
    )]))
    .unwrap();
    let outcome = store
        .update("app", "people", &older, &raise, UpdateOptions::default())
        .unwrap();
    assert_eq!((outcome.matched, outcome.modified), (1, 1));
    let by_id = parse_filter(&doc(vec![("_id", Bson::Int64(1))])).unwrap();
    assert_eq!(
        store
            .find("app", "people", &by_id, &SortSpec::default())
            .unwrap()
            .len(),
        1
    );
    let nobody = parse_filter(&doc(vec![("name", text("Linus"))])).unwrap();
    let upsert = store
        .update(
            "app",
            "people",
            &nobody,
            &raise,
            UpdateOptions {
                multi: false,
                upsert: true,
            },
        )
        .unwrap();
    assert!(upsert.upserted.is_some());
    let linus = store
        .find("app", "people", &nobody, &SortSpec::default())
        .unwrap();
    assert_eq!(linus[0].get("age"), Some(&Bson::Int32(1)));
    assert_eq!(
        store
            .delete(
                "app",
                "people",
                &parse_filter(&Document::new()).unwrap(),
                true
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .delete(
                "app",
                "people",
                &parse_filter(&Document::new()).unwrap(),
                false
            )
            .unwrap(),
        2
    );
}

#[test]
fn a_store_lists_creates_and_drops_namespaces() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    store.create_collection("app", "a").unwrap();
    assert_eq!(store.create_collection("app", "a").unwrap_err().code, 48);
    store
        .insert("other", "b", vec![Document::new()], true)
        .unwrap();
    let names: Vec<String> = store
        .databases()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, vec!["app", "other"]);
    assert_eq!(
        store.create_collection("bad.name", "a").unwrap_err().code,
        73
    );
    assert!(store.drop_collection("app", "a").unwrap());
    assert!(!store.drop_collection("app", "a").unwrap());
    store.drop_database("other").unwrap();
    assert!(store.databases().unwrap().is_empty());
}

#[test]
fn find_and_modify_returns_before_or_after() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    store
        .insert(
            "app",
            "counters",
            vec![doc(vec![("_id", text("hits")), ("n", Bson::Int32(1))])],
            true,
        )
        .unwrap();
    let filter = parse_filter(&doc(vec![("_id", text("hits"))])).unwrap();
    let update = parse_update(&doc(vec![(
        "$inc",
        Bson::Document(doc(vec![("n", Bson::Int32(1))])),
    )]))
    .unwrap();
    let (after, _) = store
        .find_and_modify(
            "app",
            "counters",
            &filter,
            &SortSpec::default(),
            &Modification::Update {
                update: update.clone(),
                upsert: false,
                new: true,
            },
        )
        .unwrap();
    assert_eq!(after.unwrap().get("n"), Some(&Bson::Int32(2)));
    let (before, _) = store
        .find_and_modify(
            "app",
            "counters",
            &filter,
            &SortSpec::default(),
            &Modification::Update {
                update,
                upsert: false,
                new: false,
            },
        )
        .unwrap();
    assert_eq!(before.unwrap().get("n"), Some(&Bson::Int32(2)));
    let (removed, _) = store
        .find_and_modify(
            "app",
            "counters",
            &filter,
            &SortSpec::default(),
            &Modification::Remove,
        )
        .unwrap();
    assert!(removed.is_some());
}

#[test]
fn indexes_are_recorded_and_listed() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let index = doc(vec![
        ("key", Bson::Document(doc(vec![("name", Bson::Int32(1))]))),
        ("name", text("name_1")),
    ]);
    assert_eq!(
        store.create_indexes("app", "people", &[index]).unwrap(),
        (1, 2)
    );
    assert_eq!(store.indexes("app", "people").unwrap().unwrap().len(), 2);
    assert_eq!(store.drop_indexes("app", "people", "name_1").unwrap(), 2);
    assert_eq!(
        store
            .drop_indexes("app", "people", "nope")
            .unwrap_err()
            .code,
        27
    );
}

#[test]
fn extended_json_reads_canonical_and_relaxed_forms() {
    let parsed = crate::parse_extended_json(
        r#"{"_id": {"$oid": "5f1b2c3d4e5f60718293a4b5"}, "n": 1, "big": {"$numberLong": "9007199254740993"}, "d": 2.5,
            "when": {"$date": "2020-02-29T12:30:00.250Z"}, "old": {"$date": {"$numberLong": "-1000"}},
            "bin": {"$binary": {"base64": "AQID", "subType": "04"}}, "re": {"$regularExpression": {"pattern": "^a", "options": "i"}},
            "nested": {"list": [true, null, {"$numberDouble": "Infinity"}]}}"#,
    )
    .unwrap();
    assert_eq!(
        parsed.get("_id"),
        Some(&Bson::ObjectId([
            0x5f, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5
        ]))
    );
    assert_eq!(parsed.get("n"), Some(&Bson::Int32(1)));
    assert_eq!(parsed.get("big"), Some(&Bson::Int64(9_007_199_254_740_993)));
    assert_eq!(parsed.get("when"), Some(&Bson::DateTime(1_582_979_400_250)));
    assert_eq!(parsed.get("old"), Some(&Bson::DateTime(-1000)));
    assert_eq!(
        parsed.get("bin"),
        Some(&Bson::Binary {
            subtype: 4,
            bytes: vec![1, 2, 3]
        })
    );
    assert!(crate::parse_extended_json(r#"{"_id": {"$oid": "zz"}}"#).is_err());
    assert!(crate::parse_extended_json("[1]").is_err());
    assert!(crate::parse_extended_json(&"[".repeat(500)).is_err());
}

#[test]
fn a_mongodump_collection_restores_with_its_indexes() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let mut dump = Vec::new();
    for n in 0..2_500 {
        doc(vec![
            ("_id", Bson::Int32(n)),
            ("name", text(&format!("user {n}"))),
        ])
        .write(&mut dump);
    }
    doc(vec![("_id", Bson::Int32(7))]).write(&mut dump);
    let metadata = r#"{"indexes": [{"v": {"$numberInt": "2"}, "key": {"_id": {"$numberInt": "1"}}, "name": "_id_"},
        {"v": 2, "key": {"name": 1}, "name": "name_1"}, {"v": 2, "key": {"email": 1}, "name": "email_1", "unique": true}],
        "uuid": "0123", "collectionName": "users"}"#;
    let restored =
        crate::restore_collection(&store, "app", "users", dump.as_slice(), Some(metadata)).unwrap();
    assert_eq!(
        (restored.documents, restored.failed, restored.indexes),
        (2_500, 1, 1)
    );
    assert!(restored.first_error.unwrap().contains("E11000"));
    assert_eq!(restored.skipped_indexes.len(), 1);
    let truncated = &dump[..dump.len() - 3];
    assert!(crate::restore_collection(&store, "app", "broken", truncated, None).is_err());
    assert!(
        crate::restore_collection(
            &store,
            "app",
            "hostile",
            [0xff, 0xff, 0xff, 0x7f].as_slice(),
            None
        )
        .is_err()
    );
}

fn nested(levels: usize) -> Bson {
    (0..levels).fold(Bson::Int32(1), |inner, _| {
        Bson::Document(doc(vec![("a", inner)]))
    })
}

#[test]
fn updates_that_would_nest_too_deeply_are_refused_and_change_nothing() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    store
        .insert(
            "app",
            "deep",
            vec![doc(vec![("_id", Bson::Int32(1))])],
            true,
        )
        .unwrap();
    let everyone = parse_filter(&Document::new()).unwrap();
    let long_path = vec!["a"; 20_000].join(".");
    let by_path = parse_update(&doc(vec![(
        "$set",
        Bson::Document(doc(vec![(long_path.as_str(), Bson::Int32(1))])),
    )]))
    .unwrap();
    assert!(
        store
            .update("app", "deep", &everyone, &by_path, UpdateOptions::default())
            .is_err()
    );
    let by_value = parse_update(&doc(vec![(
        "$set",
        Bson::Document(doc(vec![("w.x.y.z", nested(97))])),
    )]))
    .unwrap();
    let refused = store
        .update(
            "app",
            "deep",
            &everyone,
            &by_value,
            UpdateOptions::default(),
        )
        .unwrap_err();
    assert!(refused.message.contains("nesting depth"), "{refused:?}");
    let found = store
        .find("app", "deep", &everyone, &SortSpec::default())
        .unwrap();
    assert_eq!(found, vec![doc(vec![("_id", Bson::Int32(1))])]);
}

fn sub(entries: Vec<(&str, Bson)>) -> Bson {
    Bson::Document(doc(entries))
}

fn ints(values: &[i32]) -> Bson {
    Bson::Array(values.iter().map(|value| Bson::Int32(*value)).collect())
}

fn changed_by(
    document: Document,
    query: Document,
    update: Bson,
    array_filters: Option<Bson>,
) -> Result<Document, DocumentError> {
    let update = parse_update_statement(&update, array_filters.as_ref())?;
    let query = parse_filter(&query)?;
    let mut document = document;
    apply_update(&update, &mut document, Some(&query), false, 1_000)?;
    Ok(document)
}

fn refused_statement(update: Bson, array_filters: Option<Bson>) -> DocumentError {
    parse_update_statement(&update, array_filters.as_ref()).unwrap_err()
}

fn grade(grade: i32, mean: i32, std: i32) -> Bson {
    sub(vec![
        ("grade", Bson::Int32(grade)),
        ("mean", Bson::Int32(mean)),
        ("std", Bson::Int32(std)),
    ])
}

#[test]
fn the_positional_operator_changes_the_element_the_query_matched() {
    let students = doc(vec![
        ("_id", Bson::Int32(1)),
        ("grades", ints(&[85, 80, 80])),
    ]);
    let changed = changed_by(
        students.clone(),
        doc(vec![("_id", Bson::Int32(1)), ("grades", Bson::Int32(80))]),
        sub(vec![("$set", sub(vec![("grades.$", Bson::Int32(82))]))]),
        None,
    )
    .unwrap();
    assert_eq!(changed.get("grades"), Some(&ints(&[85, 82, 80])));

    let fourth = doc(vec![
        ("_id", Bson::Int32(4)),
        (
            "grades",
            Bson::Array(vec![grade(80, 75, 8), grade(85, 90, 5), grade(85, 85, 8)]),
        ),
    ]);
    let changed = changed_by(
        fourth,
        doc(vec![
            ("_id", Bson::Int32(4)),
            ("grades.grade", Bson::Int32(85)),
        ]),
        sub(vec![("$set", sub(vec![("grades.$.std", Bson::Int32(6))]))]),
        None,
    )
    .unwrap();
    assert_eq!(
        changed.get("grades"),
        Some(&Bson::Array(vec![
            grade(80, 75, 8),
            grade(85, 90, 6),
            grade(85, 85, 8)
        ]))
    );

    let fifth = doc(vec![
        ("_id", Bson::Int32(5)),
        (
            "grades",
            Bson::Array(vec![grade(80, 75, 8), grade(85, 90, 5), grade(90, 85, 3)]),
        ),
    ]);
    let changed = changed_by(
        fifth,
        doc(vec![
            ("_id", Bson::Int32(5)),
            (
                "grades",
                sub(vec![(
                    "$elemMatch",
                    sub(vec![
                        ("grade", sub(vec![("$lte", Bson::Int32(90))])),
                        ("mean", sub(vec![("$gt", Bson::Int32(80))])),
                    ]),
                )]),
            ),
        ]),
        sub(vec![("$set", sub(vec![("grades.$.std", Bson::Int32(6))]))]),
        None,
    )
    .unwrap();
    assert_eq!(
        changed.get("grades"),
        Some(&Bson::Array(vec![
            grade(80, 75, 8),
            grade(85, 90, 6),
            grade(90, 85, 3)
        ]))
    );

    let unmatched = changed_by(
        students,
        doc(vec![("_id", Bson::Int32(1))]),
        sub(vec![("$set", sub(vec![("grades.$", Bson::Int32(1))]))]),
        None,
    )
    .unwrap_err();
    assert_eq!(
        (unmatched.code, unmatched.message.as_str()),
        (
            2,
            "The positional operator did not find the match needed from the query."
        )
    );
}

#[test]
fn all_positional_and_filtered_positional_paths_change_every_matching_element() {
    let everyone = Document::new();
    let changed = changed_by(
        doc(vec![
            ("_id", Bson::Int32(1)),
            ("grades", ints(&[85, 82, 80])),
        ]),
        everyone.clone(),
        sub(vec![("$inc", sub(vec![("grades.$[]", Bson::Int32(10))]))]),
        None,
    )
    .unwrap();
    assert_eq!(changed.get("grades"), Some(&ints(&[95, 92, 90])));

    let changed = changed_by(
        doc(vec![
            ("_id", Bson::Int32(1)),
            (
                "grades",
                Bson::Array(vec![grade(80, 75, 8), grade(85, 90, 5), grade(85, 85, 8)]),
            ),
        ]),
        everyone.clone(),
        sub(vec![(
            "$inc",
            sub(vec![("grades.$[].std", Bson::Int32(-2))]),
        )]),
        None,
    )
    .unwrap();
    assert_eq!(
        changed.get("grades"),
        Some(&Bson::Array(vec![
            grade(80, 75, 6),
            grade(85, 90, 3),
            grade(85, 85, 6)
        ]))
    );

    let changed = changed_by(
        doc(vec![
            ("_id", Bson::Int32(2)),
            ("grades", ints(&[98, 100, 102])),
        ]),
        everyone.clone(),
        sub(vec![(
            "$set",
            sub(vec![("grades.$[element]", Bson::Int32(100))]),
        )]),
        Some(Bson::Array(vec![sub(vec![(
            "element",
            sub(vec![("$gte", Bson::Int32(100))]),
        )])])),
    )
    .unwrap();
    assert_eq!(changed.get("grades"), Some(&ints(&[98, 100, 100])));

    let changed = changed_by(
        doc(vec![
            ("_id", Bson::Int32(1)),
            (
                "grades",
                Bson::Array(vec![grade(80, 75, 6), grade(85, 90, 4), grade(85, 85, 6)]),
            ),
        ]),
        everyone,
        sub(vec![(
            "$set",
            sub(vec![("grades.$[elem].mean", Bson::Int32(100))]),
        )]),
        Some(Bson::Array(vec![sub(vec![(
            "elem.grade",
            sub(vec![("$gte", Bson::Int32(85))]),
        )])])),
    )
    .unwrap();
    assert_eq!(
        changed.get("grades"),
        Some(&Bson::Array(vec![
            grade(80, 75, 6),
            grade(85, 100, 4),
            grade(85, 100, 6)
        ]))
    );
}

fn questions(kind: &str, scores: &[i32]) -> Bson {
    sub(vec![("type", text(kind)), ("questions", ints(scores))])
}

fn quizzes() -> Document {
    doc(vec![
        ("_id", Bson::Int32(1)),
        (
            "grades",
            Bson::Array(vec![
                questions("quiz", &[10, 8, 5]),
                questions("quiz", &[8, 9, 6]),
                questions("hw", &[5, 4, 3]),
                questions("exam", &[25, 10, 23, 0]),
            ]),
        ),
    ])
}

#[test]
fn nested_identifiers_filter_each_array_level() {
    let changed = changed_by(
        quizzes(),
        Document::new(),
        sub(vec![(
            "$inc",
            sub(vec![("grades.$[t].questions.$[score]", Bson::Int32(2))]),
        )]),
        Some(Bson::Array(vec![
            sub(vec![("t.type", text("quiz"))]),
            sub(vec![("score", sub(vec![("$gte", Bson::Int32(8))]))]),
        ])),
    )
    .unwrap();
    assert_eq!(
        changed.get("grades"),
        Some(&Bson::Array(vec![
            questions("quiz", &[12, 10, 5]),
            questions("quiz", &[10, 11, 6]),
            questions("hw", &[5, 4, 3]),
            questions("exam", &[25, 10, 23, 0]),
        ]))
    );

    let changed = changed_by(
        quizzes(),
        Document::new(),
        sub(vec![(
            "$inc",
            sub(vec![("grades.$[].questions.$[score]", Bson::Int32(2))]),
        )]),
        Some(Bson::Array(vec![sub(vec![(
            "score",
            sub(vec![("$gte", Bson::Int32(8))]),
        )])])),
    )
    .unwrap();
    assert_eq!(
        changed.get("grades"),
        Some(&Bson::Array(vec![
            questions("quiz", &[12, 10, 5]),
            questions("quiz", &[10, 11, 6]),
            questions("hw", &[5, 4, 3]),
            questions("exam", &[27, 12, 25, 0]),
        ]))
    );
}

#[test]
fn positional_updates_are_refused_with_the_documented_codes() {
    let set = |path: &str| sub(vec![("$set", sub(vec![(path, Bson::Int32(1))]))]);
    let filters = |entries: Vec<Bson>| Some(Bson::Array(entries));

    let missing = refused_statement(set("grades.$[x]"), None);
    assert_eq!(
        (missing.code, missing.message.as_str()),
        (
            2,
            "No array filter found for identifier 'x' in path 'grades.$[x]'"
        )
    );
    let unused = refused_statement(
        set("grades.$[x]"),
        filters(vec![
            sub(vec![("x", Bson::Int32(1))]),
            sub(vec![("y", Bson::Int32(2))]),
        ]),
    );
    assert_eq!(unused.code, 9);
    assert!(
        unused
            .message
            .starts_with("The array filter for identifier 'y' was not used in the update"),
        "{unused:?}"
    );
    let unused_by_replacement = refused_statement(
        sub(vec![("name", text("Ada"))]),
        filters(vec![sub(vec![("x", Bson::Int32(1))])]),
    );
    assert_eq!(unused_by_replacement.code, 9);
    let capital = refused_statement(
        set("grades.$[X]"),
        filters(vec![sub(vec![("X", Bson::Int32(1))])]),
    );
    assert_eq!(capital.code, 2);
    assert!(
        capital
            .message
            .ends_with("beginning with a lowercase letter, found 'X'"),
        "{capital:?}"
    );
    let two_names = refused_statement(
        set("grades.$[x]"),
        filters(vec![sub(vec![
            ("x", Bson::Int32(1)),
            ("y", Bson::Int32(2)),
        ])]),
    );
    assert_eq!(two_names.code, 9);
    let twice = refused_statement(
        set("grades.$[x]"),
        filters(vec![
            sub(vec![("x", Bson::Int32(1))]),
            sub(vec![("x", Bson::Int32(2))]),
        ]),
    );
    assert_eq!(twice.code, 9);
    assert_eq!(refused_statement(set("a.$.b.$"), None).code, 2);
    assert_eq!(refused_statement(set("$[]"), None).code, 2);
    assert_eq!(
        refused_statement(
            sub(vec![("$rename", sub(vec![("a.$[]", text("b"))]))]),
            None
        )
        .code,
        2
    );
    let pipeline_with_filters = refused_statement(
        Bson::Array(vec![sub(vec![("$set", sub(vec![("a", Bson::Int32(1))]))])]),
        filters(Vec::new()),
    );
    assert_eq!(
        (pipeline_with_filters.code, pipeline_with_filters.code_name),
        (72, "InvalidOptions")
    );
    let match_stage =
        refused_statement(Bson::Array(vec![sub(vec![("$match", sub(vec![]))])]), None);
    assert_eq!(match_stage.code, 72);

    let numbers = doc(vec![("_id", Bson::Int32(1)), ("grades", ints(&[85, 80]))]);
    let not_viable =
        changed_by(numbers.clone(), Document::new(), set("grades.$[].x"), None).unwrap_err();
    assert_eq!(
        (not_viable.code, not_viable.code_name),
        (28, "PathNotViable")
    );
    let no_array =
        changed_by(numbers.clone(), Document::new(), set("missing.$[]"), None).unwrap_err();
    assert_eq!(
        (no_array.code, no_array.message.as_str()),
        (
            2,
            "The path 'missing' must exist in the document in order to apply array updates."
        )
    );
    let not_array = changed_by(numbers.clone(), Document::new(), set("_id.$[]"), None).unwrap_err();
    assert_eq!(not_array.code, 2);
    let conflict = changed_by(
        numbers,
        Document::new(),
        sub(vec![
            ("$set", sub(vec![("grades.$[]", Bson::Int32(1))])),
            ("$inc", sub(vec![("grades.0", Bson::Int32(1))])),
        ]),
        None,
    )
    .unwrap_err();
    assert_eq!(conflict.code, 40);
}

#[test]
fn pipeline_updates_run_their_stages_over_the_document() {
    let students = doc(vec![
        ("_id", Bson::Int32(3)),
        ("test1", Bson::Int32(95)),
        ("test2", Bson::Int32(110)),
        ("modified", Bson::DateTime(0)),
    ]);
    let changed = changed_by(
        students.clone(),
        doc(vec![("_id", Bson::Int32(3))]),
        Bson::Array(vec![sub(vec![(
            "$set",
            sub(vec![
                ("test3", Bson::Int32(98)),
                ("modified", text("$$NOW")),
            ]),
        )])]),
        None,
    )
    .unwrap();
    assert_eq!(changed.get("test3"), Some(&Bson::Int32(98)));
    assert_eq!(changed.get("modified"), Some(&Bson::DateTime(1_000)));

    let defaults = changed_by(
        doc(vec![
            ("_id", Bson::Int32(1)),
            ("quiz1", Bson::Int32(8)),
            ("test2", Bson::Int32(100)),
            ("quiz2", Bson::Int32(9)),
        ]),
        Document::new(),
        Bson::Array(vec![
            sub(vec![(
                "$replaceRoot",
                sub(vec![(
                    "newRoot",
                    sub(vec![(
                        "$mergeObjects",
                        Bson::Array(vec![
                            sub(vec![
                                ("quiz1", Bson::Int32(0)),
                                ("quiz2", Bson::Int32(0)),
                                ("test1", Bson::Int32(0)),
                                ("test2", Bson::Int32(0)),
                            ]),
                            text("$$ROOT"),
                        ]),
                    )]),
                )]),
            )]),
            sub(vec![("$set", sub(vec![("modified", text("$$NOW"))]))]),
        ]),
        None,
    )
    .unwrap();
    assert_eq!(defaults.first().map(|(name, _)| name.as_str()), Some("_id"));
    assert_eq!(defaults.get("quiz1"), Some(&Bson::Int32(8)));
    assert_eq!(defaults.get("quiz2"), Some(&Bson::Int32(9)));
    assert_eq!(defaults.get("test1"), Some(&Bson::Int32(0)));
    assert_eq!(defaults.get("test2"), Some(&Bson::Int32(100)));
    assert_eq!(defaults.get("modified"), Some(&Bson::DateTime(1_000)));

    let unset = changed_by(
        students.clone(),
        Document::new(),
        Bson::Array(vec![sub(vec![(
            "$unset",
            Bson::Array(vec![text("test1"), text("modified")]),
        )])]),
        None,
    )
    .unwrap();
    assert_eq!(
        unset,
        doc(vec![("_id", Bson::Int32(3)), ("test2", Bson::Int32(110))])
    );
    let projected = changed_by(
        students.clone(),
        Document::new(),
        Bson::Array(vec![sub(vec![(
            "$project",
            sub(vec![("test2", Bson::Int32(1))]),
        )])]),
        None,
    )
    .unwrap();
    assert_eq!(projected, unset);

    let longer = changed_by(
        doc(vec![("_id", Bson::Int32(2)), ("quizzes", ints(&[5, 6, 7]))]),
        Document::new(),
        Bson::Array(vec![sub(vec![(
            "$set",
            sub(vec![(
                "quizzes",
                sub(vec![(
                    "$concatArrays",
                    Bson::Array(vec![text("$quizzes"), ints(&[8, 6])]),
                )]),
            )]),
        )])]),
        None,
    )
    .unwrap();
    assert_eq!(longer.get("quizzes"), Some(&ints(&[5, 6, 7, 8, 6])));

    let new_id = changed_by(
        students,
        Document::new(),
        Bson::Array(vec![sub(vec![(
            "$set",
            sub(vec![("_id", Bson::Int32(9))]),
        )])]),
        None,
    )
    .unwrap_err();
    assert_eq!(new_id.code, 66);
}

#[test]
fn a_pipeline_upsert_starts_from_the_query_equalities() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let filter =
        parse_filter(&doc(vec![("_id", Bson::Int32(7)), ("name", text("Grace"))])).unwrap();
    let update = parse_update_statement(
        &Bson::Array(vec![sub(vec![(
            "$set",
            sub(vec![("visits", Bson::Int32(1))]),
        )])]),
        None,
    )
    .unwrap();
    assert!(matches!(update, Update::Pipeline(_)));
    let outcome = store
        .update(
            "app",
            "people",
            &filter,
            &update,
            UpdateOptions {
                multi: true,
                upsert: true,
            },
        )
        .unwrap();
    assert_eq!(outcome.upserted, Some(Bson::Int32(7)));
    let found = store
        .find("app", "people", &filter, &SortSpec::default())
        .unwrap();
    assert_eq!(
        found,
        vec![doc(vec![
            ("_id", Bson::Int32(7)),
            ("name", text("Grace")),
            ("visits", Bson::Int32(1)),
        ])]
    );
}

const MEBIBYTE: usize = 1024 * 1024;

fn reported_size(error: &DocumentError) -> usize {
    error
        .message
        .split("size in bytes: ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|size| size.parse().ok())
        .unwrap_or(usize::MAX)
}

fn assert_refused_early(error: &DocumentError) {
    assert_eq!((error.code, error.code_name), (2, "BadValue"));
    assert!(
        error.message.starts_with("object to insert too large"),
        "{}",
        error.message
    );
    let size = reported_size(error);
    assert!(
        size > crate::MAX_DOCUMENT_BYTES && size <= crate::MAX_DOCUMENT_BYTES + 2 * MEBIBYTE,
        "the update was built to {size} bytes before it was refused"
    );
}

fn stored_with_array(store: &Store<'_>, elements: Vec<Bson>) -> Document {
    let document = doc(vec![
        ("_id", Bson::Int32(1)),
        ("s", text(&"x".repeat(MEBIBYTE))),
        ("a", Bson::Array(elements)),
    ]);
    store
        .insert("app", "big", vec![document.clone()], true)
        .unwrap();
    document
}

fn update_stored(store: &Store<'_>, update: Bson) -> Result<(), DocumentError> {
    let update = parse_update_statement(&update, None)?;
    store
        .update(
            "app",
            "big",
            &parse_filter(&Document::new()).unwrap(),
            &update,
            UpdateOptions::default(),
        )
        .map(|_| ())
}

fn stored(store: &Store<'_>) -> Vec<Document> {
    store
        .find(
            "app",
            "big",
            &parse_filter(&Document::new()).unwrap(),
            &SortSpec::default(),
        )
        .unwrap()
}

#[test]
fn the_counted_size_is_the_encoded_size() {
    let mut document = person();
    document.insert(
        "many",
        Bson::Array((0..1_234).map(Bson::Int32).collect::<Vec<_>>()),
    );
    document.insert(
        "bytes",
        Bson::Binary {
            subtype: 0,
            bytes: vec![7; 33],
        },
    );
    assert_eq!(
        crate::size::document_size(&document),
        document.to_bytes().len()
    );
}

#[test]
fn an_update_that_copies_its_argument_past_the_limit_is_refused_early() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let before = stored_with_array(&store, vec![Bson::Int32(0); 64]);
    let big = text(&"y".repeat(MEBIBYTE));
    let set_all = sub(vec![("$set", sub(vec![("a.$[]", big.clone())]))]);
    assert_refused_early(&update_stored(&store, set_all).unwrap_err());
    assert_eq!(stored(&store), vec![before.clone()]);

    let filtered = parse_update_statement(
        &sub(vec![("$set", sub(vec![("a.$[zero]", big.clone())]))]),
        Some(&Bson::Array(vec![sub(vec![("zero", Bson::Int32(0))])])),
    )
    .unwrap();
    let error = store
        .update(
            "app",
            "big",
            &parse_filter(&Document::new()).unwrap(),
            &filtered,
            UpdateOptions::default(),
        )
        .unwrap_err();
    assert_refused_early(&error);
    assert_eq!(stored(&store), vec![before]);

    update_stored(
        &store,
        sub(vec![("$set", sub(vec![("a.$[]", Bson::Int32(5))]))]),
    )
    .unwrap();
    assert_eq!(
        stored(&store)[0].get("a"),
        Some(&Bson::Array(vec![Bson::Int32(5); 64]))
    );
}

#[test]
fn push_and_add_to_set_with_each_over_many_arrays_are_refused_early() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let before = stored_with_array(&store, vec![Bson::Array(Vec::new()); 64]);
    let each = sub(vec![(
        "$each",
        Bson::Array(vec![
            text(&"z".repeat(MEBIBYTE / 2)),
            text(&"w".repeat(MEBIBYTE / 2)),
        ]),
    )]);
    for operator in ["$push", "$addToSet"] {
        let update = sub(vec![(operator, sub(vec![("a.$[]", each.clone())]))]);
        assert_refused_early(&update_stored(&store, update).unwrap_err());
        assert_eq!(stored(&store), vec![before.clone()]);
    }
    update_stored(
        &store,
        sub(vec![("$push", sub(vec![("a.$[]", Bson::Int32(1))]))]),
    )
    .unwrap();
    assert_eq!(
        stored(&store)[0].get("a"),
        Some(&Bson::Array(vec![ints(&[1]); 64]))
    );
}

#[test]
fn pipeline_expressions_that_grow_past_the_limit_are_refused_early() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let before = stored_with_array(&store, vec![Bson::Int32(0); 4]);
    let copies = |reference: &str| Bson::Array(vec![text(reference); 40]);
    let one_field =
        |expression: Bson| Bson::Array(vec![sub(vec![("$set", sub(vec![("b", expression)]))])]);
    let mut many_fields = Document::new();
    for at in 0..40 {
        many_fields.insert(format!("f{at}"), text("$s"));
    }
    let updates = [
        one_field(sub(vec![("$concat", copies("$s"))])),
        one_field(sub(vec![(
            "$concatArrays",
            Bson::Array(vec![Bson::Array(vec![text("$s")]); 40]),
        )])),
        one_field(copies("$s")),
        one_field(sub(vec![("$sum", copies("$s"))])),
        Bson::Array(vec![sub(vec![(
            "$set",
            Bson::Document(many_fields.clone()),
        )])]),
        Bson::Array(vec![sub(vec![("$project", Bson::Document(many_fields))])]),
    ];
    for update in updates {
        assert_refused_early(&update_stored(&store, update).unwrap_err());
        assert_eq!(stored(&store), vec![before.clone()]);
    }
    update_stored(
        &store,
        one_field(sub(vec![("$concat", Bson::Array(vec![text("$s"); 2]))])),
    )
    .unwrap();
    assert_eq!(
        stored(&store)[0].get("b"),
        Some(&text(&"x".repeat(2 * MEBIBYTE)))
    );
}

#[test]
fn aggregate_pipelines_bind_now_and_cluster_time_once() {
    let pipeline = vec![
        sub(vec![(
            "$addFields",
            sub(vec![("at", text("$$NOW")), ("clock", text("$$CLUSTER_TIME"))]),
        )]),
        sub(vec![(
            "$match",
            sub(vec![("name", text("$$NOW")), ("$expr", text("$$NOW"))]),
        )]),
    ];
    let bound = crate::pipeline_with_time(&pipeline, 5_000);
    let Bson::Document(first) = &bound[0] else {
        panic!("{bound:?}");
    };
    let fields = first.get_document("$addFields").unwrap();
    assert_eq!(
        fields.get("at"),
        Some(&sub(vec![("$literal", Bson::DateTime(5_000))]))
    );
    assert_eq!(
        fields.get("clock"),
        Some(&sub(vec![(
            "$literal",
            Bson::Timestamp {
                time: 5,
                increment: 1
            }
        )]))
    );
    let Bson::Document(second) = &bound[1] else {
        panic!("{bound:?}");
    };
    let filter = second.get_document("$match").unwrap();
    assert_eq!(filter.get("name"), Some(&text("$$NOW")));
    assert_eq!(
        filter.get("$expr"),
        Some(&sub(vec![("$literal", Bson::DateTime(5_000))]))
    );
}

#[test]
fn bucket_groups_by_boundaries_and_facet_runs_each_sub_pipeline() {
    let input: Vec<Document> = [5, 15, 25, 40]
        .iter()
        .map(|price| doc(vec![("price", Bson::Int32(*price))]))
        .collect();
    let pipeline = vec![sub(vec![(
        "$facet",
        sub(vec![
            (
                "byPrice",
                Bson::Array(vec![sub(vec![(
                    "$bucket",
                    sub(vec![
                        ("groupBy", text("$price")),
                        (
                            "boundaries",
                            Bson::Array(vec![Bson::Int32(0), Bson::Int32(10), Bson::Int32(30)]),
                        ),
                        ("default", text("Other")),
                    ]),
                )])]),
            ),
            (
                "total",
                Bson::Array(vec![sub(vec![("$count", text("n"))])]),
            ),
        ]),
    )])];
    let stages = crate::parse_pipeline(&pipeline).unwrap();
    let no_other = |_: &str| -> crate::DocumentResult<Vec<Document>> { Ok(Vec::new()) };
    let out = crate::run_pipeline(&stages, input, &no_other).unwrap();
    assert_eq!(out.len(), 1);
    let Some(Bson::Array(buckets)) = out[0].get("byPrice") else {
        panic!("{out:?}");
    };
    let counts: Vec<(Bson, Bson)> = buckets
        .iter()
        .filter_map(Bson::as_document)
        .map(|bucket| {
            (
                bucket.get("_id").cloned().unwrap_or(Bson::Null),
                bucket.get("count").cloned().unwrap_or(Bson::Null),
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![
            (Bson::Int32(0), Bson::Int32(1)),
            (Bson::Int32(10), Bson::Int32(2)),
            (text("Other"), Bson::Int32(1)),
        ]
    );
    let Some(Bson::Array(total)) = out[0].get("total") else {
        panic!("{out:?}");
    };
    assert_eq!(total[0].as_document().and_then(|count| count.get("n")), Some(&Bson::Int32(4)));
}

#[test]
fn expr_filter_compares_two_fields_of_the_document() {
    let filter = crate::parse_filter(&Document::from_entries(vec![(
        "$expr".to_owned(),
        sub(vec![("$gt", Bson::Array(vec![text("$spent"), text("$budget")]))]),
    )]))
    .unwrap();
    let over = doc(vec![("spent", Bson::Int32(12)), ("budget", Bson::Int32(10))]);
    let under = doc(vec![("spent", Bson::Int32(8)), ("budget", Bson::Int32(10))]);
    assert!(crate::matches(&filter, &over));
    assert!(!crate::matches(&filter, &under));
}

#[test]
fn lookup_pipeline_binds_let_variables_for_each_document() {
    let orders = vec![
        doc(vec![("item", text("almonds")), ("ordered", Bson::Int32(12))]),
        doc(vec![("item", text("pecans")), ("ordered", Bson::Int32(1))]),
    ];
    let stock = vec![
        doc(vec![("sku", text("almonds")), ("instock", Bson::Int32(120))]),
        doc(vec![("sku", text("almonds")), ("instock", Bson::Int32(5))]),
        doc(vec![("sku", text("pecans")), ("instock", Bson::Int32(70))]),
    ];
    let pipeline = vec![sub(vec![(
        "$lookup",
        sub(vec![
            ("from", text("warehouses")),
            (
                "let",
                sub(vec![("order_item", text("$item")), ("order_qty", text("$ordered"))]),
            ),
            (
                "pipeline",
                Bson::Array(vec![
                    sub(vec![(
                        "$match",
                        sub(vec![(
                            "$expr",
                            sub(vec![(
                                "$and",
                                Bson::Array(vec![
                                    sub(vec![("$eq", Bson::Array(vec![text("$sku"), text("$$order_item")]))]),
                                    sub(vec![("$gte", Bson::Array(vec![text("$instock"), text("$$order_qty")]))]),
                                ]),
                            )]),
                        )]),
                    )]),
                    sub(vec![("$project", sub(vec![("sku", Bson::Int32(0))]))]),
                ]),
            ),
            ("as", text("stockdata")),
        ]),
    )])];
    let stages = crate::parse_pipeline(&pipeline).unwrap();
    let load = |name: &str| -> crate::DocumentResult<Vec<Document>> {
        assert_eq!(name, "warehouses");
        Ok(stock.clone())
    };
    let out = crate::run_pipeline(&stages, orders, &load).unwrap();
    let joined: Vec<usize> = out
        .iter()
        .map(|order| match order.get("stockdata") {
            Some(Bson::Array(items)) => items.len(),
            _ => usize::MAX,
        })
        .collect();
    assert_eq!(joined, vec![1, 1]);
    let first = out[0].get("stockdata").and_then(|items| match items {
        Bson::Array(items) => items[0].as_document().cloned(),
        _ => None,
    });
    assert_eq!(first.and_then(|stock| stock.get("instock").cloned()), Some(Bson::Int32(120)));
}

#[test]
fn lookup_refuses_a_let_name_that_starts_with_a_capital() {
    let pipeline = vec![sub(vec![(
        "$lookup",
        sub(vec![
            ("from", text("other")),
            ("let", sub(vec![("Item", text("$item"))])),
            ("pipeline", Bson::Array(Vec::new())),
            ("as", text("joined")),
        ]),
    )])];
    assert!(crate::parse_pipeline(&pipeline).is_err());
}

#[test]
fn projection_operators_slice_match_and_compute() {
    let order = doc(vec![
        ("_id", Bson::Int32(7)),
        ("customer", text("Ada")),
        (
            "items",
            Bson::Array(vec![
                sub(vec![("sku", text("a")), ("qty", Bson::Int32(1))]),
                sub(vec![("sku", text("b")), ("qty", Bson::Int32(5))]),
                sub(vec![("sku", text("c")), ("qty", Bson::Int32(9))]),
            ]),
        ),
        ("price", Bson::Int32(4)),
    ]);
    let sliced = parse_projection(&doc(vec![("items", sub(vec![("$slice", Bson::Int32(-1))]))])).unwrap();
    let shown = sliced.apply(&order);
    assert_eq!(shown.get("customer"), Some(&text("Ada")));
    assert_eq!(
        shown.get("items"),
        Some(&Bson::Array(vec![sub(vec![("sku", text("c")), ("qty", Bson::Int32(9))])]))
    );
    let matched = parse_projection(&doc(vec![(
        "items",
        sub(vec![("$elemMatch", sub(vec![("qty", sub(vec![("$gt", Bson::Int32(2))]))]))]),
    )]))
    .unwrap();
    assert_eq!(
        matched.apply(&order),
        doc(vec![
            ("_id", Bson::Int32(7)),
            ("items", Bson::Array(vec![sub(vec![("sku", text("b")), ("qty", Bson::Int32(5))])])),
        ])
    );
    let query = parse_filter(&doc(vec![("items.sku", text("c"))])).unwrap();
    let positional = parse_projection(&doc(vec![("items.$", Bson::Int32(1))])).unwrap();
    assert_eq!(
        positional.apply_matching(&order, &query).get("items"),
        Some(&Bson::Array(vec![sub(vec![("sku", text("c")), ("qty", Bson::Int32(9))])]))
    );
    let computed = parse_projection(&doc(vec![
        ("customer", Bson::Int32(1)),
        ("doubled", sub(vec![("$multiply", Bson::Array(vec![text("$price"), Bson::Int32(2)]))])),
    ]))
    .unwrap();
    assert_eq!(
        computed.apply(&order),
        doc(vec![("_id", Bson::Int32(7)), ("customer", text("Ada")), ("doubled", Bson::Int32(8))])
    );
    assert!(parse_projection(&doc(vec![("items.$", Bson::Int32(0))])).is_err());
}

#[test]
fn json_schema_filters_by_types_bounds_and_required_fields() {
    let filter = parse_filter(&doc(vec![(
        "$jsonSchema",
        sub(vec![
            ("bsonType", text("object")),
            ("required", Bson::Array(vec![text("name"), text("age")])),
            (
                "properties",
                sub(vec![
                    ("name", sub(vec![("bsonType", text("string")), ("minLength", Bson::Int32(2))])),
                    (
                        "age",
                        sub(vec![
                            ("bsonType", text("int")),
                            ("minimum", Bson::Int32(0)),
                            ("maximum", Bson::Int32(150)),
                        ]),
                    ),
                    ("tags", sub(vec![("bsonType", text("array")), ("uniqueItems", Bson::Boolean(true))])),
                ]),
            ),
        ]),
    )]))
    .unwrap();
    let good = doc(vec![("name", text("Ada")), ("age", Bson::Int32(36)), ("tags", Bson::Array(vec![text("a"), text("b")]))]);
    let missing = doc(vec![("name", text("Ada"))]);
    let too_old = doc(vec![("name", text("Ada")), ("age", Bson::Int32(200))]);
    let repeated = doc(vec![("name", text("Ada")), ("age", Bson::Int32(1)), ("tags", Bson::Array(vec![text("a"), text("a")]))]);
    assert!(crate::matches(&filter, &good));
    assert!(!crate::matches(&filter, &missing));
    assert!(!crate::matches(&filter, &too_old));
    assert!(!crate::matches(&filter, &repeated));
    assert!(parse_filter(&doc(vec![("$jsonSchema", sub(vec![("format", text("email"))]))])).is_err());
    assert!(parse_filter(&doc(vec![("$jsonSchema", sub(vec![("bogus", Bson::Int32(1))]))])).is_err());
}
