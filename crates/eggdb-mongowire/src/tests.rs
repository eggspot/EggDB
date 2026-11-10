use crate::commands::{Shared, run};
use crate::cursors::Cursors;
use crate::message::{OP_MSG, OP_QUERY};
use crate::server::{ServerConfig, Session, serve_stream};
use eggdb_auth::scram::ScramVerifier;
use eggdb_bson::{Bson, Document};
use eggdb_documents::Store;
use eggdb_keyspace::MemoryKeyspace;
use std::sync::Mutex;
use std::time::Instant;

const PASSWORD: &str = "mongo-secret";

fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = (at as u8).wrapping_mul(31).wrapping_add(7);
    }
    Ok(())
}

fn config() -> ServerConfig {
    ServerConfig {
        user: "eggdb".to_owned(),
        verifier: ScramVerifier::derive(PASSWORD.as_bytes(), b"0123456789abcdef", 4096),
        unknown_user_secret: [9; 32],
        max_connections: 4,
        random: fill,
        memory_budget: std::sync::Arc::default(),
    }
}

fn shared<'s>(store: &'s Store<'s>, config: &'s ServerConfig) -> Shared<'s> {
    Shared {
        store,
        config,
        cursors: Mutex::new(Cursors::new(7, &config.memory_budget)),
        started: Instant::now(),
    }
}

fn session(authenticated: bool) -> Session {
    Session {
        id: 1,
        peer: "127.0.0.1:5000".to_owned(),
        authenticated_user: authenticated.then(|| "eggdb".to_owned()),
        conversation: None,
        failed_logins: 0,
    }
}

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

fn sub(entries: Vec<(&str, Bson)>) -> Bson {
    Bson::Document(doc(entries))
}

fn ok(reply: &Document) -> bool {
    reply.get("ok").and_then(Bson::as_f64) == Some(1.0)
}

fn batch<'d>(reply: &'d Document, name: &str) -> &'d Vec<Bson> {
    reply
        .get_document("cursor")
        .and_then(|cursor| cursor.get_array(name))
        .expect("a cursor batch")
}

struct Harness<'s> {
    shared: Shared<'s>,
    session: Session,
}

impl Harness<'_> {
    fn run(&mut self, body: Document) -> Document {
        run(&self.shared, &mut self.session, "app", &body)
    }

    fn run_in(&mut self, database: &str, body: Document) -> Document {
        run(&self.shared, &mut self.session, database, &body)
    }
}

fn with_harness(test: impl FnOnce(&mut Harness<'_>)) {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let config = config();
    let mut harness = Harness {
        shared: shared(&store, &config),
        session: session(true),
    };
    test(&mut harness);
}

#[test]
fn commands_need_authentication_except_the_handshake() {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let config = config();
    let shared = shared(&store, &config);
    let mut session = session(false);
    let hello = run(
        &shared,
        &mut session,
        "admin",
        &doc(vec![
            ("hello", Bson::Int32(1)),
            ("saslSupportedMechs", text("admin.eggdb")),
        ]),
    );
    assert!(ok(&hello));
    assert_eq!(
        hello.get_array("saslSupportedMechs"),
        Some(&vec![text("SCRAM-SHA-256")])
    );
    assert_eq!(hello.get("maxWireVersion"), Some(&Bson::Int32(17)));
    let refused = run(
        &shared,
        &mut session,
        "app",
        &doc(vec![("find", text("people"))]),
    );
    assert_eq!(refused.get("code"), Some(&Bson::Int32(13)));
}

fn scram_client_final(server_first: &str, client_first_bare: &str, password: &str) -> String {
    let attributes: Vec<(&str, &str)> = server_first
        .split(',')
        .filter_map(|part| part.split_once('='))
        .collect();
    let find = |name: &str| {
        attributes
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
            .expect("attribute")
    };
    let nonce = find("r");
    let salt = eggdb_crypto::base64_decode(find("s").as_bytes()).expect("salt");
    let iterations: u32 = find("i").parse().expect("iterations");
    let mut salted = [0u8; 32];
    eggdb_crypto::pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, &mut salted);
    let client_key = eggdb_crypto::hmac_sha256(&salted, b"Client Key");
    let stored_key = eggdb_crypto::sha256(&client_key);
    let without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    let signature = eggdb_crypto::hmac_sha256(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key
        .iter()
        .zip(signature)
        .map(|(key, sign)| key ^ sign)
        .collect();
    format!("{without_proof},p={}", eggdb_crypto::base64_encode(&proof))
}

fn authenticate(
    shared: &Shared<'_>,
    session: &mut Session,
    password: &str,
    skip_empty: bool,
) -> Document {
    let bare = "n=eggdb,r=fyko+d2lbbFgONRv9qkxdawL";
    let mut start = doc(vec![
        ("saslStart", Bson::Int32(1)),
        ("mechanism", text("SCRAM-SHA-256")),
        (
            "payload",
            Bson::Binary {
                subtype: 0,
                bytes: format!("n,,{bare}").into_bytes(),
            },
        ),
    ]);
    if skip_empty {
        start.insert(
            "options",
            sub(vec![("skipEmptyExchange", Bson::Boolean(true))]),
        );
    }
    let first = run(shared, session, "admin", &start);
    assert!(ok(&first), "{first:?}");
    let Some(Bson::Binary { bytes, .. }) = first.get("payload") else {
        panic!("no payload");
    };
    let server_first = String::from_utf8(bytes.clone()).expect("UTF-8");
    let final_message = scram_client_final(&server_first, bare, password);
    let mut reply = run(
        shared,
        session,
        "admin",
        &doc(vec![
            ("saslContinue", Bson::Int32(1)),
            ("conversationId", Bson::Int32(1)),
            (
                "payload",
                Bson::Binary {
                    subtype: 0,
                    bytes: final_message.into_bytes(),
                },
            ),
        ]),
    );
    if ok(&reply) && reply.get_bool("done") == Some(false) {
        reply = run(
            shared,
            session,
            "admin",
            &doc(vec![
                ("saslContinue", Bson::Int32(1)),
                ("conversationId", Bson::Int32(1)),
                (
                    "payload",
                    Bson::Binary {
                        subtype: 0,
                        bytes: Vec::new(),
                    },
                ),
            ]),
        );
    }
    reply
}

#[test]
fn scram_sha_256_logs_in_with_the_right_password_only() {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let config = config();
    let shared = shared(&store, &config);
    for skip_empty in [true, false] {
        let mut good = session(false);
        let reply = authenticate(&shared, &mut good, PASSWORD, skip_empty);
        assert!(
            ok(&reply) && reply.get_bool("done") == Some(true),
            "{reply:?}"
        );
        assert_eq!(good.authenticated_user.as_deref(), Some("eggdb"));
    }
    let mut bad = session(false);
    let reply = authenticate(&shared, &mut bad, "wrong", true);
    assert_eq!(reply.get("code"), Some(&Bson::Int32(18)));
    assert!(bad.authenticated_user.is_none());
}

#[test]
fn documents_are_inserted_found_paged_and_projected() {
    with_harness(|harness| {
        let people: Vec<Bson> = (0..150)
            .map(|n| {
                sub(vec![
                    ("_id", Bson::Int32(n)),
                    ("n", Bson::Int32(n)),
                    ("even", Bson::Boolean(n % 2 == 0)),
                ])
            })
            .collect();
        let inserted = harness.run(doc(vec![
            ("insert", text("people")),
            ("documents", Bson::Array(people)),
        ]));
        assert_eq!(inserted.get("n"), Some(&Bson::Int32(150)));
        let found = harness.run(doc(vec![
            ("find", text("people")),
            ("filter", sub(vec![("even", Bson::Boolean(true))])),
            ("sort", sub(vec![("n", Bson::Int32(-1))])),
            (
                "projection",
                sub(vec![("n", Bson::Int32(1)), ("_id", Bson::Int32(0))]),
            ),
            ("batchSize", Bson::Int32(50)),
        ]));
        let first = batch(&found, "firstBatch");
        assert_eq!(first.len(), 50);
        assert_eq!(first[0], sub(vec![("n", Bson::Int32(148))]));
        let id = found
            .get_document("cursor")
            .and_then(|cursor| cursor.get("id"))
            .and_then(Bson::as_i64)
            .unwrap();
        assert_ne!(id, 0);
        let more = harness.run(doc(vec![
            ("getMore", Bson::Int64(id)),
            ("collection", text("people")),
        ]));
        assert_eq!(batch(&more, "nextBatch").len(), 25);
        assert_eq!(
            more.get_document("cursor")
                .and_then(|cursor| cursor.get("id")),
            Some(&Bson::Int64(0))
        );
        let gone = harness.run(doc(vec![
            ("getMore", Bson::Int64(id)),
            ("collection", text("people")),
        ]));
        assert_eq!(gone.get("code"), Some(&Bson::Int32(43)));
        let limited = harness.run(doc(vec![
            ("find", text("people")),
            ("skip", Bson::Int32(10)),
            ("limit", Bson::Int32(3)),
        ]));
        assert_eq!(batch(&limited, "firstBatch").len(), 3);
    });
}

#[test]
fn updates_deletes_and_find_and_modify_report_counts() {
    with_harness(|harness| {
        harness.run(doc(vec![
            ("insert", text("items")),
            (
                "documents",
                Bson::Array(vec![
                    sub(vec![("_id", Bson::Int32(1)), ("qty", Bson::Int32(5))]),
                    sub(vec![("_id", Bson::Int32(2)), ("qty", Bson::Int32(0))]),
                ]),
            ),
        ]));
        let updated = harness.run(doc(vec![
            ("update", text("items")),
            (
                "updates",
                Bson::Array(vec![
                    sub(vec![
                        ("q", sub(vec![])),
                        ("u", sub(vec![("$inc", sub(vec![("qty", Bson::Int32(1))]))])),
                        ("multi", Bson::Boolean(true)),
                    ]),
                    sub(vec![
                        ("q", sub(vec![("_id", Bson::Int32(9))])),
                        ("u", sub(vec![("$set", sub(vec![("qty", Bson::Int32(9))]))])),
                        ("upsert", Bson::Boolean(true)),
                    ]),
                ]),
            ),
        ]));
        assert_eq!(updated.get("n"), Some(&Bson::Int32(3)));
        assert_eq!(updated.get("nModified"), Some(&Bson::Int32(2)));
        assert_eq!(updated.get_array("upserted").map(Vec::len), Some(1));
        let modified = harness.run(doc(vec![
            ("findAndModify", text("items")),
            ("query", sub(vec![("_id", Bson::Int32(1))])),
            (
                "update",
                sub(vec![("$set", sub(vec![("qty", Bson::Int32(100))]))]),
            ),
            ("new", Bson::Boolean(true)),
        ]));
        assert_eq!(
            modified
                .get_document("value")
                .and_then(|value| value.get("qty")),
            Some(&Bson::Int32(100))
        );
        let deleted = harness.run(doc(vec![
            ("delete", text("items")),
            (
                "deletes",
                Bson::Array(vec![sub(vec![
                    ("q", sub(vec![("qty", sub(vec![("$lt", Bson::Int32(10))]))])),
                    ("limit", Bson::Int32(0)),
                ])]),
            ),
        ]));
        assert_eq!(deleted.get("n"), Some(&Bson::Int32(2)));
        let counted = harness.run(doc(vec![("count", text("items"))]));
        assert_eq!(counted.get("n"), Some(&Bson::Int32(1)));
        let duplicate = harness.run(doc(vec![
            ("insert", text("items")),
            (
                "documents",
                Bson::Array(vec![sub(vec![("_id", Bson::Int32(1))])]),
            ),
        ]));
        assert_eq!(
            duplicate
                .get_array("writeErrors")
                .and_then(|errors| errors[0].as_document())
                .and_then(|error| error.get("code")),
            Some(&Bson::Int32(11000))
        );
    });
}

#[test]
fn aggregation_groups_unwinds_and_looks_up() {
    with_harness(|harness| {
        harness.run(doc(vec![
            ("insert", text("orders")),
            (
                "documents",
                Bson::Array(vec![
                    sub(vec![
                        ("_id", Bson::Int32(1)),
                        ("customer", text("ada")),
                        ("items", Bson::Array(vec![text("pen"), text("ink")])),
                        ("total", Bson::Int32(30)),
                    ]),
                    sub(vec![
                        ("_id", Bson::Int32(2)),
                        ("customer", text("ada")),
                        ("items", Bson::Array(vec![text("pad")])),
                        ("total", Bson::Int32(12)),
                    ]),
                    sub(vec![
                        ("_id", Bson::Int32(3)),
                        ("customer", text("bob")),
                        ("items", Bson::Array(vec![])),
                        ("total", Bson::Double(7.5)),
                    ]),
                ]),
            ),
        ]));
        harness.run(doc(vec![
            ("insert", text("customers")),
            (
                "documents",
                Bson::Array(vec![sub(vec![
                    ("_id", text("ada")),
                    ("city", text("London")),
                ])]),
            ),
        ]));
        let grouped = harness.run(doc(vec![
            ("aggregate", text("orders")),
            (
                "pipeline",
                Bson::Array(vec![
                    sub(vec![(
                        "$match",
                        sub(vec![("total", sub(vec![("$gt", Bson::Int32(5))]))]),
                    )]),
                    sub(vec![(
                        "$group",
                        sub(vec![
                            ("_id", text("$customer")),
                            ("spent", sub(vec![("$sum", text("$total"))])),
                            ("orders", sub(vec![("$count", sub(vec![]))])),
                        ]),
                    )]),
                    sub(vec![("$sort", sub(vec![("spent", Bson::Int32(-1))]))]),
                    sub(vec![(
                        "$lookup",
                        sub(vec![
                            ("from", text("customers")),
                            ("localField", text("_id")),
                            ("foreignField", text("_id")),
                            ("as", text("profile")),
                        ]),
                    )]),
                    sub(vec![(
                        "$project",
                        sub(vec![
                            ("spent", Bson::Int32(1)),
                            ("orders", Bson::Int32(1)),
                            (
                                "city",
                                sub(vec![(
                                    "$arrayElemAt",
                                    Bson::Array(vec![text("$profile.city"), Bson::Int32(0)]),
                                )]),
                            ),
                        ]),
                    )]),
                ]),
            ),
            ("cursor", sub(vec![])),
        ]));
        assert!(ok(&grouped), "{grouped:?}");
        let rows = batch(&grouped, "firstBatch");
        assert_eq!(
            rows[0],
            sub(vec![
                ("_id", text("ada")),
                ("spent", Bson::Int32(42)),
                ("orders", Bson::Int32(2)),
                ("city", text("London"))
            ])
        );
        assert_eq!(
            rows[1],
            sub(vec![
                ("_id", text("bob")),
                ("spent", Bson::Double(7.5)),
                ("orders", Bson::Int32(1))
            ])
        );
        let unwound = harness.run(doc(vec![
            ("aggregate", text("orders")),
            (
                "pipeline",
                Bson::Array(vec![
                    sub(vec![("$unwind", text("$items"))]),
                    sub(vec![("$count", text("lines"))]),
                ]),
            ),
            ("cursor", sub(vec![])),
        ]));
        assert_eq!(
            batch(&unwound, "firstBatch")[0],
            sub(vec![("lines", Bson::Int32(3))])
        );
        let counted = harness.run(doc(vec![
            ("aggregate", text("orders")),
            (
                "pipeline",
                Bson::Array(vec![
                    sub(vec![("$match", sub(vec![]))]),
                    sub(vec![(
                        "$group",
                        sub(vec![
                            ("_id", Bson::Int32(1)),
                            ("n", sub(vec![("$sum", Bson::Int32(1))])),
                        ]),
                    )]),
                ]),
            ),
            ("cursor", sub(vec![])),
        ]));
        assert_eq!(
            batch(&counted, "firstBatch")[0]
                .as_document()
                .and_then(|row| row.get("n")),
            Some(&Bson::Int32(3))
        );
        let unknown = harness.run(doc(vec![
            ("aggregate", text("orders")),
            (
                "pipeline",
                Bson::Array(vec![sub(vec![("$bogus", sub(vec![]))])]),
            ),
            ("cursor", sub(vec![])),
        ]));
        assert!(!ok(&unknown));
    });
}

#[test]
fn aggregation_writes_its_results_with_out_and_merge() {
    with_harness(|harness| {
        assert!(ok(&harness.run(doc(vec![
            ("insert", text("sales")),
            (
                "documents",
                Bson::Array(vec![
                    sub(vec![("_id", Bson::Int32(1)), ("region", text("north")), ("amount", Bson::Int32(5))]),
                    sub(vec![("_id", Bson::Int32(2)), ("region", text("north")), ("amount", Bson::Int32(7))]),
                    sub(vec![("_id", Bson::Int32(3)), ("region", text("south")), ("amount", Bson::Int32(4))]),
                ]),
            ),
        ]))));
        let totals = || {
            sub(vec![(
                "$group",
                sub(vec![("_id", text("$region")), ("total", sub(vec![("$sum", text("$amount"))]))]),
            )])
        };
        let out = harness.run(doc(vec![
            ("aggregate", text("sales")),
            ("pipeline", Bson::Array(vec![totals(), sub(vec![("$out", text("totals"))])])),
            ("cursor", sub(vec![])),
        ]));
        assert!(ok(&out));
        assert!(batch(&out, "firstBatch").is_empty());
        let found = harness.run(doc(vec![
            ("find", text("totals")),
            ("filter", sub(vec![("_id", text("north"))])),
        ]));
        assert_eq!(
            batch(&found, "firstBatch")[0].as_document().and_then(|row| row.get("total")),
            Some(&Bson::Int32(12))
        );
        let merged = harness.run(doc(vec![
            ("aggregate", text("sales")),
            (
                "pipeline",
                Bson::Array(vec![
                    sub(vec![("$match", sub(vec![("region", text("south"))]))]),
                    totals(),
                    sub(vec![(
                        "$merge",
                        sub(vec![("into", text("totals")), ("whenMatched", text("fail"))]),
                    )]),
                ]),
            ),
            ("cursor", sub(vec![])),
        ]));
        assert_eq!(merged.get("code"), Some(&Bson::Int32(11000)));
        let misplaced = harness.run(doc(vec![
            ("aggregate", text("sales")),
            (
                "pipeline",
                Bson::Array(vec![sub(vec![("$out", text("totals"))]), totals()]),
            ),
            ("cursor", sub(vec![])),
        ]));
        assert_eq!(misplaced.get("code"), Some(&Bson::Int32(40601)));
    });
}

#[test]
fn collections_are_renamed_through_the_admin_database() {
    with_harness(|harness| {
        assert!(ok(&harness.run(doc(vec![
            ("insert", text("drafts")),
            ("documents", Bson::Array(vec![sub(vec![("_id", Bson::Int32(1))])])),
        ]))));
        let renamed = harness.run_in(
            "admin",
            doc(vec![("renameCollection", text("app.drafts")), ("to", text("app.posts"))]),
        );
        assert!(ok(&renamed));
        let found = harness.run(doc(vec![("find", text("posts"))]));
        assert_eq!(batch(&found, "firstBatch").len(), 1);
        assert!(batch(&harness.run(doc(vec![("find", text("drafts"))])), "firstBatch").is_empty());
        let missing = harness.run_in(
            "admin",
            doc(vec![("renameCollection", text("app.drafts")), ("to", text("app.posts"))]),
        );
        assert_eq!(missing.get("code"), Some(&Bson::Int32(26)));
    });
}

#[test]
fn explain_reports_a_collection_scan_or_an_id_lookup() {
    with_harness(|harness| {
        assert!(ok(&harness.run(doc(vec![
            ("insert", text("logs")),
            (
                "documents",
                Bson::Array(vec![
                    sub(vec![("_id", Bson::Int32(1)), ("level", text("warn"))]),
                    sub(vec![("_id", Bson::Int32(2)), ("level", text("info"))]),
                ]),
            ),
        ]))));
        let scanned = harness.run(doc(vec![
            (
                "explain",
                sub(vec![("find", text("logs")), ("filter", sub(vec![("level", text("warn"))]))]),
            ),
            ("verbosity", text("executionStats")),
        ]));
        assert!(ok(&scanned));
        let planner = scanned.get_document("queryPlanner").cloned().unwrap_or_default();
        assert_eq!(
            planner.get_document("winningPlan").and_then(|plan| plan.get_str("stage")),
            Some("COLLSCAN")
        );
        let statistics = scanned.get_document("executionStats").cloned().unwrap_or_default();
        assert_eq!(statistics.get("nReturned"), Some(&Bson::Int64(1)));
        assert_eq!(statistics.get("totalDocsExamined"), Some(&Bson::Int64(2)));
        let by_id = harness.run(doc(vec![(
            "explain",
            sub(vec![("find", text("logs")), ("filter", sub(vec![("_id", Bson::Int32(2))]))]),
        )]));
        assert_eq!(
            by_id
                .get_document("queryPlanner")
                .and_then(|planner| planner.get_document("winningPlan"))
                .and_then(|plan| plan.get_str("stage")),
            Some("IDHACK")
        );
    });
}

#[test]
fn namespaces_and_indexes_are_listed() {
    with_harness(|harness| {
        assert!(ok(&harness.run(doc(vec![("create", text("logs"))]))));
        assert_eq!(
            harness.run(doc(vec![("create", text("logs"))])).get("code"),
            Some(&Bson::Int32(48))
        );
        let created = harness.run(doc(vec![
            ("createIndexes", text("logs")),
            (
                "indexes",
                Bson::Array(vec![sub(vec![
                    ("key", sub(vec![("at", Bson::Int32(-1))])),
                    ("name", text("at_-1")),
                ])]),
            ),
        ]));
        assert_eq!(created.get("numIndexesAfter"), Some(&Bson::Int32(2)));
        let listed = harness.run(doc(vec![("listIndexes", text("logs"))]));
        assert_eq!(batch(&listed, "firstBatch").len(), 2);
        let collections = harness.run(doc(vec![
            ("listCollections", Bson::Int32(1)),
            ("nameOnly", Bson::Boolean(true)),
        ]));
        assert_eq!(
            batch(&collections, "firstBatch")[0],
            sub(vec![("name", text("logs")), ("type", text("collection"))])
        );
        let databases = harness.run(doc(vec![("listDatabases", Bson::Int32(1))]));
        assert_eq!(databases.get_array("databases").map(Vec::len), Some(1));
        assert!(ok(&harness.run(doc(vec![("drop", text("logs"))]))));
        assert_eq!(
            harness
                .run(doc(vec![("nonsense", Bson::Int32(1))]))
                .get("code"),
            Some(&Bson::Int32(59))
        );
        assert_eq!(
            harness
                .run(doc(vec![("find", text("bad$name"))]))
                .get("code"),
            Some(&Bson::Int32(73))
        );
    });
}

fn op_msg(
    request_id: i32,
    body: &Document,
    flags: u32,
    sequence: Option<(&str, &[Document])>,
) -> Vec<u8> {
    let mut out = vec![0; 4];
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&OP_MSG.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.push(0);
    body.write(&mut out);
    if let Some((identifier, documents)) = sequence {
        out.push(1);
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(identifier.as_bytes());
        out.push(0);
        for document in documents {
            document.write(&mut out);
        }
        let size = (out.len() - start) as i32;
        out[start..start + 4].copy_from_slice(&size.to_le_bytes());
    }
    let length = out.len() as i32;
    out[..4].copy_from_slice(&length.to_le_bytes());
    out
}

fn op_query(request_id: i32, namespace: &str, query: &Document) -> Vec<u8> {
    let mut out = vec![0; 4];
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&OP_QUERY.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(namespace.as_bytes());
    out.push(0);
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&(-1i32).to_le_bytes());
    query.write(&mut out);
    let length = out.len() as i32;
    out[..4].copy_from_slice(&length.to_le_bytes());
    out
}

fn replies(bytes: &[u8]) -> Vec<(i32, i32, Document)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let length = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let message = &bytes[at..at + length];
        let response_to = i32::from_le_bytes(message[8..12].try_into().unwrap());
        let op_code = i32::from_le_bytes(message[12..16].try_into().unwrap());
        let document_at = if op_code == OP_MSG { 21 } else { 36 };
        out.push((
            op_code,
            response_to,
            Document::from_bytes(&message[document_at..]).unwrap(),
        ));
        at += length;
    }
    out
}

#[test]
fn the_wire_framing_answers_each_request() {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let config = config();
    let shared = shared(&store, &config);
    let mut session = session(true);
    let mut input = op_query(1, "admin.$cmd", &doc(vec![("isMaster", Bson::Int32(1))]));
    input.extend(op_msg(
        2,
        &doc(vec![("insert", text("t")), ("$db", text("app"))]),
        0,
        Some((
            "documents",
            &[
                doc(vec![("_id", Bson::Int32(1))]),
                doc(vec![("_id", Bson::Int32(2))]),
            ],
        )),
    ));
    input.extend(op_msg(
        3,
        &doc(vec![
            ("insert", text("t")),
            (
                "documents",
                Bson::Array(vec![sub(vec![("_id", Bson::Int32(3))])]),
            ),
            ("$db", text("app")),
        ]),
        2,
        None,
    ));
    input.extend(op_msg(
        4,
        &doc(vec![
            ("count", text("t")),
            ("$db", text("app")),
            ("lsid", sub(vec![])),
        ]),
        0,
        None,
    ));
    let mut output = Vec::new();
    serve_stream(&shared, &mut session, input.as_slice(), &mut output).unwrap();
    let answered = replies(&output);
    assert_eq!(answered.len(), 3);
    assert_eq!(
        (answered[0].0, answered[0].1),
        (crate::message::OP_REPLY, 1)
    );
    assert_eq!(answered[0].2.get("ismaster"), Some(&Bson::Boolean(true)));
    assert_eq!(answered[1].2.get("n"), Some(&Bson::Int32(2)));
    assert_eq!(
        (answered[2].1, answered[2].2.get("n")),
        (4, Some(&Bson::Int32(3)))
    );
}

#[test]
fn hostile_messages_close_the_connection_without_panicking() {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let config = config();
    let shared = shared(&store, &config);
    let valid = op_msg(
        1,
        &doc(vec![("ping", Bson::Int32(1)), ("$db", text("admin"))]),
        0,
        Some(("x", &[doc(vec![])])),
    );
    let mut cases: Vec<Vec<u8>> = vec![
        vec![
            0xff, 0xff, 0xff, 0x7f, 0, 0, 0, 0, 0, 0, 0, 0, 0xdd, 0x07, 0, 0,
        ],
        vec![4, 0, 0, 0],
        op_msg(1, &doc(vec![]), 1 << 5, None),
    ];
    for length in 0..valid.len() {
        let mut truncated = valid[..length].to_vec();
        if truncated.len() >= 4 {
            truncated[..4].copy_from_slice(&(length as i32).to_le_bytes());
        }
        cases.push(truncated);
    }
    for at in 16..valid.len() {
        let mut corrupted = valid.clone();
        corrupted[at] ^= 0xa5;
        cases.push(corrupted);
    }
    for case in cases {
        let mut session = session(true);
        let mut output = Vec::new();
        let _ = serve_stream(&shared, &mut session, case.as_slice(), &mut output);
    }
}

#[test]
fn messages_and_cursors_past_the_memory_budget_fail_with_exceeded_memory_limit() {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let budget = std::sync::Arc::new(eggdb_platform::connection_limits::MemoryBudget::new(
        256 * 1024,
    ));
    let config = ServerConfig {
        memory_budget: std::sync::Arc::clone(&budget),
        ..config()
    };
    let shared = shared(&store, &config);

    let mut oversized = Vec::new();
    oversized.extend_from_slice(&(300 * 1024i32).to_le_bytes());
    oversized.extend_from_slice(&9i32.to_le_bytes());
    oversized.extend_from_slice(&0i32.to_le_bytes());
    oversized.extend_from_slice(&OP_MSG.to_le_bytes());
    let mut output = Vec::new();
    serve_stream(
        &shared,
        &mut session(true),
        oversized.as_slice(),
        &mut output,
    )
    .unwrap();
    let refused = replies(&output);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].1, 9);
    assert_eq!(refused[0].2.get("code"), Some(&Bson::Int32(146)));
    assert_eq!(budget.used_bytes(), 0);

    let value = "x".repeat(10 * 1024);
    let mut input = Vec::new();
    for round in 0..4 {
        let documents: Vec<Document> = (0..10)
            .map(|index| {
                doc(vec![
                    ("_id", Bson::Int32(round * 10 + index)),
                    ("v", text(&value)),
                ])
            })
            .collect();
        input.extend(op_msg(
            10 + round,
            &doc(vec![("insert", text("t")), ("$db", text("app"))]),
            0,
            Some(("documents", &documents)),
        ));
    }
    input.extend(op_msg(
        20,
        &doc(vec![
            ("find", text("t")),
            ("batchSize", Bson::Int32(1)),
            ("$db", text("app")),
        ]),
        0,
        None,
    ));
    let mut output = Vec::new();
    serve_stream(&shared, &mut session(true), input.as_slice(), &mut output).unwrap();
    let answered = replies(&output);
    assert_eq!(answered.len(), 5);
    for inserted in &answered[..4] {
        assert_eq!(
            inserted.2.get("n"),
            Some(&Bson::Int32(10)),
            "{:?}",
            inserted.2
        );
    }
    assert_eq!(answered[4].2.get("code"), Some(&Bson::Int32(146)));
    assert_eq!(budget.used_bytes(), 0);

    let mut input = op_msg(
        30,
        &doc(vec![
            ("find", text("t")),
            ("batchSize", Bson::Int32(30)),
            ("$db", text("app")),
        ]),
        0,
        None,
    );
    input.extend(op_msg(
        31,
        &doc(vec![("count", text("t")), ("$db", text("app"))]),
        0,
        None,
    ));
    let mut output = Vec::new();
    serve_stream(&shared, &mut session(true), input.as_slice(), &mut output).unwrap();
    let answered = replies(&output);
    assert_eq!(answered.len(), 2);
    let Some(Bson::Document(cursor)) = answered[0].2.get("cursor") else {
        panic!("{:?}", answered[0].2);
    };
    let Some(Bson::Int64(id)) = cursor.get("id") else {
        panic!("{cursor:?}");
    };
    assert_ne!(*id, 0);
    assert!(budget.used_bytes() >= 10 * 10 * 1024);
    assert_eq!(answered[1].2.get("n"), Some(&Bson::Int32(40)));
    let kill = op_msg(
        32,
        &doc(vec![
            ("killCursors", text("t")),
            ("cursors", Bson::Array(vec![Bson::Int64(*id)])),
            ("$db", text("app")),
        ]),
        0,
        None,
    );
    let mut output = Vec::new();
    serve_stream(&shared, &mut session(true), kill.as_slice(), &mut output).unwrap();
    assert_eq!(budget.used_bytes(), 0);
}

fn ints(values: &[i32]) -> Bson {
    Bson::Array(values.iter().map(|value| Bson::Int32(*value)).collect())
}

fn update_one(query: Bson, update: Bson, extra: Vec<(&str, Bson)>) -> Bson {
    let mut statement = vec![("q", query), ("u", update)];
    statement.extend(extra);
    sub(statement)
}

fn first_write_error_code(reply: &Document) -> Option<&Bson> {
    reply
        .get_array("writeErrors")
        .and_then(|errors| errors.first())
        .and_then(Bson::as_document)
        .and_then(|error| error.get("code"))
}

fn grades_by_id(harness: &mut Harness<'_>) -> Vec<Bson> {
    let found = harness.run(doc(vec![
        ("find", text("students")),
        ("sort", sub(vec![("_id", Bson::Int32(1))])),
    ]));
    batch(&found, "firstBatch")
        .iter()
        .filter_map(|student| student.as_document()?.get("grades").cloned())
        .collect()
}

fn insert_students(harness: &mut Harness<'_>) {
    harness.run(doc(vec![
        ("insert", text("students")),
        (
            "documents",
            Bson::Array(vec![
                sub(vec![
                    ("_id", Bson::Int32(1)),
                    ("grades", ints(&[85, 80, 80])),
                ]),
                sub(vec![
                    ("_id", Bson::Int32(2)),
                    ("grades", ints(&[88, 90, 92])),
                ]),
                sub(vec![
                    ("_id", Bson::Int32(3)),
                    ("grades", ints(&[85, 100, 90])),
                ]),
            ]),
        ),
    ]));
}

#[test]
fn positional_updates_change_the_matched_and_filtered_elements() {
    with_harness(|harness| {
        insert_students(harness);
        let positional = harness.run(doc(vec![
            ("update", text("students")),
            (
                "updates",
                Bson::Array(vec![update_one(
                    sub(vec![("_id", Bson::Int32(1)), ("grades", Bson::Int32(80))]),
                    sub(vec![("$set", sub(vec![("grades.$", Bson::Int32(82))]))]),
                    Vec::new(),
                )]),
            ),
        ]));
        assert_eq!(positional.get("nModified"), Some(&Bson::Int32(1)));
        let every = harness.run(doc(vec![
            ("update", text("students")),
            (
                "updates",
                Bson::Array(vec![update_one(
                    sub(vec![]),
                    sub(vec![("$inc", sub(vec![("grades.$[]", Bson::Int32(10))]))]),
                    vec![("multi", Bson::Boolean(true))],
                )]),
            ),
        ]));
        assert_eq!(every.get("nModified"), Some(&Bson::Int32(3)));
        let filtered = harness.run(doc(vec![
            ("update", text("students")),
            (
                "updates",
                Bson::Array(vec![update_one(
                    sub(vec![]),
                    sub(vec![(
                        "$set",
                        sub(vec![("grades.$[element]", Bson::Int32(100))]),
                    )]),
                    vec![
                        ("multi", Bson::Boolean(true)),
                        (
                            "arrayFilters",
                            Bson::Array(vec![sub(vec![(
                                "element",
                                sub(vec![("$gte", Bson::Int32(100))]),
                            )])]),
                        ),
                    ],
                )]),
            ),
        ]));
        assert_eq!(
            (filtered.get("n"), filtered.get("nModified")),
            (Some(&Bson::Int32(3)), Some(&Bson::Int32(2)))
        );
        assert_eq!(
            grades_by_id(harness),
            vec![
                ints(&[95, 92, 90]),
                ints(&[98, 100, 100]),
                ints(&[95, 100, 100])
            ]
        );
    });
}

#[test]
fn positional_update_errors_carry_mongodb_codes() {
    with_harness(|harness| {
        insert_students(harness);
        let set = |path: &str| sub(vec![("$set", sub(vec![(path, Bson::Int32(1))]))]);
        let cases = vec![
            (update_one(sub(vec![]), set("grades.$[x]"), Vec::new()), 2),
            (
                update_one(
                    sub(vec![("_id", Bson::Int32(1))]),
                    set("grades.$"),
                    Vec::new(),
                ),
                2,
            ),
            (
                update_one(
                    sub(vec![]),
                    set("grades.$[x]"),
                    vec![(
                        "arrayFilters",
                        Bson::Array(vec![
                            sub(vec![("x", Bson::Int32(1))]),
                            sub(vec![("unused", Bson::Int32(1))]),
                        ]),
                    )],
                ),
                9,
            ),
            (
                update_one(
                    sub(vec![]),
                    Bson::Array(vec![set("a")]),
                    vec![("arrayFilters", Bson::Array(Vec::new()))],
                ),
                72,
            ),
            (update_one(sub(vec![]), set("grades.$[].x"), Vec::new()), 28),
        ];
        for (statement, code) in cases {
            let reply = harness.run(doc(vec![
                ("update", text("students")),
                ("updates", Bson::Array(vec![statement.clone()])),
            ]));
            assert!(ok(&reply), "{reply:?}");
            assert_eq!(
                first_write_error_code(&reply),
                Some(&Bson::Int32(code)),
                "{statement:?} gave {reply:?}"
            );
        }
        assert_eq!(
            grades_by_id(harness),
            vec![
                ints(&[85, 80, 80]),
                ints(&[88, 90, 92]),
                ints(&[85, 100, 90])
            ]
        );
    });
}

#[test]
fn find_and_modify_takes_pipelines_and_array_filters() {
    with_harness(|harness| {
        insert_students(harness);
        let totalled = harness.run(doc(vec![
            ("findAndModify", text("students")),
            ("query", sub(vec![("_id", Bson::Int32(2))])),
            (
                "update",
                Bson::Array(vec![sub(vec![(
                    "$set",
                    sub(vec![("total", sub(vec![("$sum", text("$grades"))]))]),
                )])]),
            ),
            ("new", Bson::Boolean(true)),
        ]));
        assert_eq!(
            totalled
                .get_document("value")
                .and_then(|value| value.get("total"))
                .and_then(Bson::as_i64),
            Some(270)
        );
        let lowered = harness.run(doc(vec![
            ("findAndModify", text("students")),
            ("query", sub(vec![("_id", Bson::Int32(3))])),
            (
                "update",
                sub(vec![(
                    "$set",
                    sub(vec![("grades.$[high]", Bson::Int32(99))]),
                )]),
            ),
            (
                "arrayFilters",
                Bson::Array(vec![sub(vec![(
                    "high",
                    sub(vec![("$gte", Bson::Int32(90))]),
                )])]),
            ),
            ("new", Bson::Boolean(true)),
        ]));
        assert_eq!(
            lowered
                .get_document("value")
                .and_then(|value| value.get("grades")),
            Some(&ints(&[85, 99, 99]))
        );
        let refused = harness.run(doc(vec![
            ("findAndModify", text("students")),
            ("query", sub(vec![("_id", Bson::Int32(3))])),
            (
                "update",
                Bson::Array(vec![sub(vec![("$set", sub(vec![("a", Bson::Int32(1))]))])]),
            ),
            ("arrayFilters", Bson::Array(Vec::new())),
        ]));
        assert_eq!(
            (refused.get("code"), refused.get("codeName")),
            (Some(&Bson::Int32(72)), Some(&text("InvalidOptions")))
        );
    });
}

#[test]
fn an_update_with_array_filters_runs_over_the_wire() {
    let keyspace = MemoryKeyspace::default();
    let store = Store::new(&keyspace, [1; 8]);
    let config = config();
    let shared = shared(&store, &config);
    let mut input = op_msg(
        1,
        &doc(vec![("insert", text("students")), ("$db", text("app"))]),
        0,
        Some((
            "documents",
            &[doc(vec![
                ("_id", Bson::Int32(1)),
                (
                    "grades",
                    Bson::Array(vec![
                        sub(vec![
                            ("type", text("quiz")),
                            ("questions", ints(&[10, 8, 5])),
                        ]),
                        sub(vec![("type", text("hw")), ("questions", ints(&[5, 4, 3]))]),
                    ]),
                ),
            ])],
        )),
    );
    input.extend(op_msg(
        2,
        &doc(vec![("update", text("students")), ("$db", text("app"))]),
        0,
        Some((
            "updates",
            &[doc(vec![
                ("q", sub(vec![])),
                (
                    "u",
                    sub(vec![(
                        "$inc",
                        sub(vec![("grades.$[t].questions.$[score]", Bson::Int32(2))]),
                    )]),
                ),
                (
                    "arrayFilters",
                    Bson::Array(vec![
                        sub(vec![("t.type", text("quiz"))]),
                        sub(vec![("score", sub(vec![("$gte", Bson::Int32(8))]))]),
                    ]),
                ),
                ("multi", Bson::Boolean(true)),
            ])],
        )),
    ));
    input.extend(op_msg(
        3,
        &doc(vec![("find", text("students")), ("$db", text("app"))]),
        0,
        None,
    ));
    let mut output = Vec::new();
    serve_stream(&shared, &mut session(true), input.as_slice(), &mut output).unwrap();
    let answered = replies(&output);
    assert_eq!(answered.len(), 3);
    assert_eq!(answered[1].2.get("nModified"), Some(&Bson::Int32(1)));
    let student = batch(&answered[2].2, "firstBatch")[0]
        .as_document()
        .unwrap();
    assert_eq!(
        student.get("grades"),
        Some(&Bson::Array(vec![
            sub(vec![
                ("type", text("quiz")),
                ("questions", ints(&[12, 10, 5])),
            ]),
            sub(vec![("type", text("hw")), ("questions", ints(&[5, 4, 3]))]),
        ]))
    );
}
