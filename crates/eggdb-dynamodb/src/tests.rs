use crate::Endpoint;
use eggdb_auth::sigv4::{SignedRequest, authorization, format_amz_date};
use eggdb_http::Request;
use eggdb_keyspace::MemoryKeyspace;

fn call(endpoint: &Endpoint<'_>, action: &str, body: &str) -> (u16, String) {
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()).unwrap();
    let mut request = Request {
        method: "POST".to_owned(),
        path: "/".to_owned(),
        query: None,
        headers: vec![
            ("host".to_owned(), "127.0.0.1:8000".to_owned()),
            ("x-amz-date".to_owned(), format_amz_date(now)),
            ("x-amz-target".to_owned(), format!("DynamoDB_20120810.{action}")),
        ],
        body: body.as_bytes().to_vec(),
    };
    let signed = SignedRequest { method: "POST", path: "/", query: None, headers: &request.headers, body: &request.body };
    let value = authorization(&signed, &["host", "x-amz-date", "x-amz-target"], "eggdb", "secret", "us-east-1", "dynamodb").unwrap();
    request.headers.push(("authorization".to_owned(), value));
    let response = endpoint.answer(&request);
    (response.status, String::from_utf8(response.body).unwrap())
}

#[test]
fn tables_items_queries_and_scans_work() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "eggdb", "secret");
    let create = r#"{"TableName":"orders","KeySchema":[{"AttributeName":"customer","KeyType":"HASH"},{"AttributeName":"number","KeyType":"RANGE"}],"AttributeDefinitions":[{"AttributeName":"customer","AttributeType":"S"},{"AttributeName":"number","AttributeType":"N"}],"BillingMode":"PAY_PER_REQUEST"}"#;
    let (status, created) = call(&endpoint, "CreateTable", create);
    assert_eq!(status, 200, "{created}");
    assert!(call(&endpoint, "CreateTable", create).1.contains("ResourceInUseException"));
    for (number, total) in [(1, "12.5"), (2, "3"), (10, "8")] {
        let item = format!(r#"{{"TableName":"orders","Item":{{"customer":{{"S":"ada"}},"number":{{"N":"{number}"}},"total":{{"N":"{total}"}}}}}}"#);
        assert_eq!(call(&endpoint, "PutItem", &item).0, 200);
    }
    call(&endpoint, "PutItem", r#"{"TableName":"orders","Item":{"customer":{"S":"bob"},"number":{"N":"1"}}}"#);
    let (_, got) = call(&endpoint, "GetItem", r#"{"TableName":"orders","Key":{"customer":{"S":"ada"},"number":{"N":"2"}}}"#);
    assert!(got.contains(r#""total":{"N":"3"}"#), "{got}");
    let (_, queried) = call(
        &endpoint,
        "Query",
        r##"{"TableName":"orders","KeyConditionExpression":"#c = :c AND #n > :n","ExpressionAttributeNames":{"#c":"customer","#n":"number"},"ExpressionAttributeValues":{":c":{"S":"ada"},":n":{"N":"1"}}}"##,
    );
    assert!(queried.contains(r#""Count":2"#) && queried.find(r#""N":"2""#) < queried.find(r#""N":"10""#), "{queried}");
    let (_, newest) = call(
        &endpoint,
        "Query",
        r#"{"TableName":"orders","KeyConditionExpression":"customer = :c","ExpressionAttributeValues":{":c":{"S":"ada"}},"ScanIndexForward":false,"Limit":1}"#,
    );
    assert!(newest.contains(r#""number":{"N":"10"}"#) && newest.contains("LastEvaluatedKey"), "{newest}");
    let (_, between) = call(
        &endpoint,
        "Query",
        r#"{"TableName":"orders","KeyConditionExpression":"customer = :c AND number BETWEEN :a AND :b","ExpressionAttributeValues":{":c":{"S":"ada"},":a":{"N":"2"},":b":{"N":"9"}}}"#,
    );
    assert!(between.contains(r#""Count":1"#), "{between}");
    let (_, updated) = call(
        &endpoint,
        "UpdateItem",
        r#"{"TableName":"orders","Key":{"customer":{"S":"ada"},"number":{"N":"2"}},"UpdateExpression":"SET status = :s REMOVE total","ExpressionAttributeValues":{":s":{"S":"paid"}},"ReturnValues":"ALL_NEW"}"#,
    );
    assert!(updated.contains(r#""status":{"S":"paid"}"#) && !updated.contains("total"), "{updated}");
    let (_, conflict) = call(&endpoint, "PutItem", r#"{"TableName":"orders","Item":{"customer":{"S":"bob"},"number":{"N":"1"}},"ConditionExpression":"attribute_not_exists(customer)"}"#);
    assert!(conflict.contains("ConditionalCheckFailedException"), "{conflict}");
    call(&endpoint, "DeleteItem", r#"{"TableName":"orders","Key":{"customer":{"S":"bob"},"number":{"N":"1"}}}"#);
    assert!(call(&endpoint, "Scan", r#"{"TableName":"orders"}"#).1.contains(r#""Count":3"#));
    assert!(call(&endpoint, "DescribeTable", r#"{"TableName":"orders"}"#).1.contains(r#""ItemCount":3"#));
    assert!(call(&endpoint, "ListTables", "{}").1.contains(r#"["orders"]"#));
    assert!(call(&endpoint, "GetItem", r#"{"TableName":"nope","Key":{}}"#).1.contains("ResourceNotFoundException"));
    assert_eq!(call(&endpoint, "DeleteTable", r#"{"TableName":"orders"}"#).0, 200);
    assert_eq!(call(&endpoint, "ListTables", "{}").1, r#"{"TableNames":[]}"#);
}

#[test]
fn numbers_sort_numerically_and_unsigned_requests_are_refused() {
    let mut keys = Vec::new();
    for number in [-5.0, -0.5, 0.0, 2.0, 10.0] {
        let mut out = Vec::new();
        crate::keys::encode(&crate::keys::KeyValue::N(number), &mut out);
        keys.push(out);
    }
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "eggdb", "secret");
    let unsigned = Request { method: "POST".to_owned(), path: "/".to_owned(), query: None, headers: vec![("x-amz-target".to_owned(), "DynamoDB_20120810.ListTables".to_owned())], body: b"{}".to_vec() };
    assert!(String::from_utf8(endpoint.answer(&unsigned).body).unwrap().contains("UnrecognizedClientException"));
}
