use super::{edifact_parse, fix_field, fix_parse, hl7_parse};
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::types::Value;
use crate::Session;

fn json(value: Value) -> String {
    match value {
        Value::Jsonb(text) => text,
        other => panic!("not jsonb: {other:?}"),
    }
}

fn with_checksum(body: &str) -> String {
    let sum = body.bytes().fold(0u32, |sum, byte| (sum + u32::from(byte)) % 256);
    format!("{body}10={sum:03}\u{1}")
}

#[test]
fn fix_messages_keep_their_tags_in_order_and_their_trailer_is_checked() {
    let order = with_checksum("8=FIX.4.4\u{1}9=24\u{1}35=D\u{1}55=EGG\u{1}54=1\u{1}38=100\u{1}");
    assert_eq!(
        json(fix_parse(&order).unwrap()),
        r#"[{"tag": 8, "value": "FIX.4.4"}, {"tag": 9, "value": "24"}, {"tag": 35, "value": "D"}, {"tag": 55, "value": "EGG"}, {"tag": 54, "value": "1"}, {"tag": 38, "value": "100"}, {"tag": 10, "value": "117"}]"#
    );
    assert_eq!(fix_field(&order, &Value::Integer(55)).unwrap(), Value::Text("EGG".to_owned()));
    assert_eq!(fix_field(&order, &Value::Integer(99)).unwrap(), Value::Null);
    assert!(fix_parse("35=D|55=EGG|").is_ok());
    for bad in [order.replace("10=117", "10=116"), order.replace("9=24", "9=25"), "35=D|55".to_owned(), "x=1".to_owned(), String::new(), "99999999999=1".to_owned()] {
        assert!(fix_parse(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn hl7_segments_split_into_fields_repetitions_and_components() {
    let message = "MSH|^~\\&|LAB|EGG|||202610311200||ORU^R01|42|P|2.5\rPID|1||123^^^H~456||Doe^Jane^\\T\\||\r\nOBX|1|NM|GLU||5.4|mmol/L\n";
    let parsed = json(hl7_parse(message).unwrap());
    assert!(parsed.starts_with(r#"[{"fields": ["|", "^~\\&", "LAB", "EGG", "", "", "202610311200", "", ["ORU", "R01"], "42", "P", "2.5"], "segment": "MSH"}"#), "{parsed}");
    assert!(parsed.contains(r#"{"fields": ["1", "", [["123", "", "", "H"], "456"], "", ["Doe", "Jane", "&"], "", ""], "segment": "PID"}"#), "{parsed}");
    assert!(parsed.ends_with(r#"{"fields": ["1", "NM", "GLU", "", "5.4", "mmol/L"], "segment": "OBX"}]"#), "{parsed}");
    for bad in ["", "PID|1", "MSH", "MSHa^~\\&|", "MSH|^^\\&|", "MSH|^~\\&|\rP!D|1"] {
        assert!(hl7_parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn edifact_segments_honour_the_service_string_advice_and_release_character() {
    let message = "UNA:+.? 'UNB+UNOC:3+SENDER+RECEIVER'\nUNH+1+ORDERS:D:96A:UN'FTX+AAI+++Price ?+ tax?: 20?'s'";
    assert_eq!(
        json(edifact_parse(message).unwrap()),
        r#"[{"tag": "UNB", "elements": [["UNOC", "3"], "SENDER", "RECEIVER"]}, {"tag": "UNH", "elements": ["1", ["ORDERS", "D", "96A", "UN"]]}, {"tag": "FTX", "elements": ["AAI", "", "", "Price + tax: 20's"]}]"#
    );
    assert_eq!(json(edifact_parse("UNA*|.# ~UNB|A*B~").unwrap()), r#"[{"tag": "UNB", "elements": [["A", "B"]]}]"#);
    for bad in ["", "   ", "UNA:+", "UNB+A?", "TOOLONG+1'"] {
        assert!(edifact_parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_parsers_are_sql_functions_that_return_jsonb() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mut outputs = Vec::new();
    session
        .execute(
            "SELECT fix_field('35=D|55=EGG|', 55) AS symbol, hl7_parse(NULL) IS NULL AS empty, edifact_parse('UNH+1''') -> 0 ->> 'tag' AS tag",
            &mut |result| outputs.push(render(&result)),
        )
        .unwrap();
    assert_eq!(outputs.pop().unwrap(), "symbol|empty|tag\nEGG|t|UNH\nSELECT 1");
}
