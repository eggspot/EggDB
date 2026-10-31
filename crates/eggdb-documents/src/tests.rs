use crate::{
    Modification, SortSpec, Store, Update, UpdateOptions, apply_update, matches, parse_filter, parse_projection,
    parse_sort, parse_update, sort_documents,
};
use eggdb_bson::{Bson, Document};
use eggdb_keyspace::MemoryKeyspace;

fn text(value: &str) -> Bson {
    Bson::String(value.to_owned())
}

fn doc(entries: Vec<(&str, Bson)>) -> Document {
    Document::from_entries(entries.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

fn person() -> Document {
    doc(vec![
        ("_id", Bson::Int32(1)),
        ("name", text("Ada")),
        ("age", Bson::Int32(36)),
        ("tags", Bson::Array(vec![text("math"), text("code")])),
        ("address", Bson::Document(doc(vec![("city", text("London")), ("zip", text("N1"))]))),
        ("scores", Bson::Array(vec![
            Bson::Document(doc(vec![("subject", text("math")), ("score", Bson::Int32(95))])),
            Bson::Document(doc(vec![("subject", text("art")), ("score", Bson::Int32(60))])),
        ])),
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
    assert!(holds(doc(vec![("age", op("$in", Bson::Array(vec![Bson::Int32(1), Bson::Int64(36)])))])));
    assert!(holds(doc(vec![("tags", op("$nin", Bson::Array(vec![text("art")])))])));
    assert!(holds(doc(vec![("tags", op("$all", Bson::Array(vec![text("code"), text("math")])))])));
    assert!(holds(doc(vec![("tags", op("$size", Bson::Int32(2)))])));
    assert!(holds(doc(vec![("missing", op("$exists", Bson::Boolean(false)))])));
    assert!(holds(doc(vec![("age", op("$type", text("number")))])));
    assert!(holds(doc(vec![("age", op("$mod", Bson::Array(vec![Bson::Int32(5), Bson::Int32(1)])))])));
    assert!(holds(doc(vec![("name", op("$regex", text("^a")))]).tap(|filter| {
        if let Some(Bson::Document(inner)) = filter.get_mut("name") {
            inner.insert("$options", text("i"));
        }
    })));
    assert!(holds(doc(vec![("age", op("$not", Bson::Document(doc(vec![("$lt", Bson::Int32(18))]))))])));
    assert!(holds(doc(vec![(
        "scores",
        op("$elemMatch", Bson::Document(doc(vec![("subject", text("math")), ("score", op("$gte", Bson::Int32(90)))]))),
    )])));
    assert!(!holds(doc(vec![(
        "scores",
        op("$elemMatch", Bson::Document(doc(vec![("subject", text("art")), ("score", op("$gte", Bson::Int32(90)))]))),
    )])));
    assert!(holds(doc(vec![(
        "$or",
        Bson::Array(vec![Bson::Document(doc(vec![("name", text("x"))])), Bson::Document(doc(vec![("age", Bson::Int32(36))]))]),
    )])));
    assert!(!holds(doc(vec![("$nor", Bson::Array(vec![Bson::Document(doc(vec![("age", Bson::Int32(36))]))]))])));
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
    assert_eq!(parse_filter(&doc(vec![("$bogus", Bson::Int32(1))])).unwrap_err().code, 2);
    assert_eq!(parse_filter(&doc(vec![("a", op("$bogus", Bson::Int32(1)))])).unwrap_err().code, 2);
    assert!(parse_filter(&doc(vec![("$or", Bson::Array(Vec::new()))])).is_err());
    assert!(parse_filter(&doc(vec![("a", op("$mod", Bson::Array(vec![Bson::Int32(0), Bson::Int32(1)])))])).is_err());
    let mut deep = doc(vec![("a", Bson::Int32(1))]);
    for _ in 0..200 {
        deep = doc(vec![("$and", Bson::Array(vec![Bson::Document(deep)]))]);
    }
    assert!(parse_filter(&deep).is_err());
}

fn updated(update: Document) -> Document {
    let mut document = person();
    apply_update(&parse_update(&update).unwrap(), &mut document, false, 1_000).unwrap();
    document
}

#[test]
fn update_operators_change_documents() {
    let op = |name: &str, fields: Vec<(&str, Bson)>| (name, Bson::Document(doc(fields)));
    let changed = updated(doc(vec![
        op("$set", vec![("address.city", text("Paris")), ("new.deep", Bson::Boolean(true))]),
        op("$inc", vec![("age", Bson::Int32(1)), ("visits", Bson::Int64(2))]),
        op("$unset", vec![("address.zip", text(""))]),
        op("$push", vec![("tags", text("poetry"))]),
        op("$currentDate", vec![("seen", Bson::Boolean(true))]),
    ]));
    assert_eq!(changed.get_document("address").unwrap().get_str("city"), Some("Paris"));
    assert!(!changed.get_document("address").unwrap().contains_key("zip"));
    assert_eq!(changed.get("age"), Some(&Bson::Int32(37)));
    assert_eq!(changed.get("visits"), Some(&Bson::Int64(2)));
    assert_eq!(changed.get_array("tags").unwrap().len(), 3);
    assert_eq!(changed.get("seen"), Some(&Bson::DateTime(1_000)));
    assert_eq!(changed.get_document("new").unwrap().get("deep"), Some(&Bson::Boolean(true)));

    let changed = updated(doc(vec![
        op("$addToSet", vec![("tags", Bson::Document(doc(vec![("$each", Bson::Array(vec![text("math"), text("art")]))])))]),
        op("$pull", vec![("scores", Bson::Document(doc(vec![("score", Bson::Document(doc(vec![("$lt", Bson::Int32(70))])))])))]),
        op("$rename", vec![("name", text("fullName"))]),
        op("$max", vec![("age", Bson::Int32(40))]),
    ]));
    assert_eq!(changed.get_array("tags").unwrap(), &vec![text("math"), text("code"), text("art")]);
    assert_eq!(changed.get_array("scores").unwrap().len(), 1);
    assert_eq!(changed.get_str("fullName"), Some("Ada"));
    assert!(!changed.contains_key("name"));
    assert_eq!(changed.get("age"), Some(&Bson::Int32(40)));
}

#[test]
fn bad_updates_are_errors_and_change_nothing() {
    let op = |name: &str, fields: Vec<(&str, Bson)>| (name, Bson::Document(doc(fields)));
    assert_eq!(parse_update(&doc(vec![op("$bogus", vec![("a", Bson::Int32(1))])])).unwrap_err().code, 9);
    assert_eq!(parse_update(&doc(vec![op("$set", vec![("a", Bson::Int32(1))]), op("$inc", vec![("a", Bson::Int32(1))])])).unwrap_err().code, 40);
    let mut document = person();
    let inc_text = parse_update(&doc(vec![op("$set", vec![("x", Bson::Int32(1))]), op("$inc", vec![("name", Bson::Int32(1))])])).unwrap();
    assert_eq!(apply_update(&inc_text, &mut document, false, 0).unwrap_err().code, 14);
    assert_eq!(document, person());
    let change_id = parse_update(&doc(vec![op("$set", vec![("_id", Bson::Int32(2))])])).unwrap();
    assert_eq!(apply_update(&change_id, &mut document, false, 0).unwrap_err().code, 66);
    let overflow = parse_update(&doc(vec![op("$inc", vec![("age", Bson::Int64(i64::MAX))])])).unwrap();
    let mut big = doc(vec![("age", Bson::Int64(2))]);
    assert!(apply_update(&overflow, &mut big, false, 0).is_err());
}

#[test]
fn a_replacement_keeps_the_id() {
    let replacement = parse_update(&doc(vec![("name", text("Grace"))])).unwrap();
    assert!(matches!(replacement, Update::Replacement(_)));
    let mut document = person();
    apply_update(&replacement, &mut document, false, 0).unwrap();
    assert_eq!(document, doc(vec![("_id", Bson::Int32(1)), ("name", text("Grace"))]));
}

#[test]
fn projections_keep_or_drop_paths() {
    let keep = parse_projection(&doc(vec![("name", Bson::Int32(1)), ("address.city", Bson::Int32(1))])).unwrap();
    assert_eq!(
        keep.apply(&person()),
        doc(vec![("_id", Bson::Int32(1)), ("name", text("Ada")), ("address", Bson::Document(doc(vec![("city", text("London"))])))])
    );
    let drop = parse_projection(&doc(vec![("scores", Bson::Int32(0)), ("_id", Bson::Int32(0)), ("tags", Bson::Int32(0))])).unwrap();
    assert_eq!(drop.apply(&person()).keys().cloned().collect::<Vec<_>>(), vec!["name", "age", "address"]);
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
        doc(vec![("v", Bson::Array(vec![Bson::Int32(10), Bson::Int32(0)]))]),
        doc(vec![("v", Bson::Double(1.5))]),
    ];
    sort_documents(&mut documents, &parse_sort(&doc(vec![("v", Bson::Int32(1))])).unwrap());
    let firsts: Vec<Option<&Bson>> = documents.iter().map(|document| document.get("v")).collect();
    assert_eq!(firsts[0], None);
    assert_eq!(firsts[1], Some(&Bson::Array(vec![Bson::Int32(10), Bson::Int32(0)])));
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
        .insert("app", "people", vec![person(), doc(vec![("name", text("Grace")), ("age", Bson::Int32(45))])], true)
        .unwrap();
    assert_eq!(outcome.inserted, 2);
    let duplicate = store.insert("app", "people", vec![doc(vec![("_id", Bson::Double(1.0))])], true).unwrap();
    assert_eq!(duplicate.errors[0].1.code, 11000);
    let everyone = store.find("app", "people", &parse_filter(&Document::new()).unwrap(), &SortSpec::default()).unwrap();
    assert_eq!(everyone.len(), 2);
    assert!(matches!(everyone[1].first(), Some((name, Bson::ObjectId(_))) if name == "_id"));
    let older = parse_filter(&doc(vec![("age", Bson::Document(doc(vec![("$gt", Bson::Int32(40))])))])).unwrap();
    let raise = parse_update(&doc(vec![("$inc", Bson::Document(doc(vec![("age", Bson::Int32(1))])))])).unwrap();
    let outcome = store.update("app", "people", &older, &raise, UpdateOptions::default()).unwrap();
    assert_eq!((outcome.matched, outcome.modified), (1, 1));
    let by_id = parse_filter(&doc(vec![("_id", Bson::Int64(1))])).unwrap();
    assert_eq!(store.find("app", "people", &by_id, &SortSpec::default()).unwrap().len(), 1);
    let nobody = parse_filter(&doc(vec![("name", text("Linus"))])).unwrap();
    let upsert = store.update("app", "people", &nobody, &raise, UpdateOptions { multi: false, upsert: true }).unwrap();
    assert!(upsert.upserted.is_some());
    let linus = store.find("app", "people", &nobody, &SortSpec::default()).unwrap();
    assert_eq!(linus[0].get("age"), Some(&Bson::Int32(1)));
    assert_eq!(store.delete("app", "people", &parse_filter(&Document::new()).unwrap(), true).unwrap(), 1);
    assert_eq!(store.delete("app", "people", &parse_filter(&Document::new()).unwrap(), false).unwrap(), 2);
}

#[test]
fn a_store_lists_creates_and_drops_namespaces() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    store.create_collection("app", "a").unwrap();
    assert_eq!(store.create_collection("app", "a").unwrap_err().code, 48);
    store.insert("other", "b", vec![Document::new()], true).unwrap();
    let names: Vec<String> = store.databases().unwrap().into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, vec!["app", "other"]);
    assert_eq!(store.create_collection("bad.name", "a").unwrap_err().code, 73);
    assert!(store.drop_collection("app", "a").unwrap());
    assert!(!store.drop_collection("app", "a").unwrap());
    store.drop_database("other").unwrap();
    assert!(store.databases().unwrap().is_empty());
}

#[test]
fn find_and_modify_returns_before_or_after() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    store.insert("app", "counters", vec![doc(vec![("_id", text("hits")), ("n", Bson::Int32(1))])], true).unwrap();
    let filter = parse_filter(&doc(vec![("_id", text("hits"))])).unwrap();
    let update = parse_update(&doc(vec![("$inc", Bson::Document(doc(vec![("n", Bson::Int32(1))])))])).unwrap();
    let (after, _) = store
        .find_and_modify("app", "counters", &filter, &SortSpec::default(), &Modification::Update { update: update.clone(), upsert: false, new: true })
        .unwrap();
    assert_eq!(after.unwrap().get("n"), Some(&Bson::Int32(2)));
    let (before, _) = store
        .find_and_modify("app", "counters", &filter, &SortSpec::default(), &Modification::Update { update, upsert: false, new: false })
        .unwrap();
    assert_eq!(before.unwrap().get("n"), Some(&Bson::Int32(2)));
    let (removed, _) = store.find_and_modify("app", "counters", &filter, &SortSpec::default(), &Modification::Remove).unwrap();
    assert!(removed.is_some());
}

#[test]
fn indexes_are_recorded_and_listed() {
    let keyspace = MemoryKeyspace::default();
    let store = store(&keyspace);
    let index = doc(vec![("key", Bson::Document(doc(vec![("name", Bson::Int32(1))]))), ("name", text("name_1"))]);
    assert_eq!(store.create_indexes("app", "people", &[index]).unwrap(), (1, 2));
    assert_eq!(store.indexes("app", "people").unwrap().unwrap().len(), 2);
    assert_eq!(store.drop_indexes("app", "people", "name_1").unwrap(), 2);
    assert_eq!(store.drop_indexes("app", "people", "nope").unwrap_err().code, 27);
}
