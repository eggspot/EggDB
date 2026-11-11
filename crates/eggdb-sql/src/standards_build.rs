use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};

pub(crate) const FUNCTIONS: [&str; 5] = [
    "hl7_build",
    "hl7_ack",
    "fix_build",
    "fix_tag_name",
    "edifact_segment_name",
];

const SOH: char = '\u{1}';

const FIX_TAGS: &[(u32, &str)] = &[
    (1, "Account"),
    (6, "AvgPx"),
    (8, "BeginString"),
    (9, "BodyLength"),
    (10, "CheckSum"),
    (11, "ClOrdID"),
    (12, "Commission"),
    (14, "CumQty"),
    (15, "Currency"),
    (17, "ExecID"),
    (18, "ExecInst"),
    (20, "ExecTransType"),
    (21, "HandlInst"),
    (22, "SecurityIDSource"),
    (31, "LastPx"),
    (32, "LastQty"),
    (34, "MsgSeqNum"),
    (35, "MsgType"),
    (36, "NewSeqNo"),
    (37, "OrderID"),
    (38, "OrderQty"),
    (39, "OrdStatus"),
    (40, "OrdType"),
    (41, "OrigClOrdID"),
    (43, "PossDupFlag"),
    (44, "Price"),
    (45, "RefSeqNum"),
    (48, "SecurityID"),
    (49, "SenderCompID"),
    (50, "SenderSubID"),
    (52, "SendingTime"),
    (54, "Side"),
    (55, "Symbol"),
    (56, "TargetCompID"),
    (57, "TargetSubID"),
    (58, "Text"),
    (59, "TimeInForce"),
    (60, "TransactTime"),
    (63, "SettlType"),
    (64, "SettlDate"),
    (65, "SymbolSfx"),
    (66, "ListID"),
    (75, "TradeDate"),
    (78, "NoAllocs"),
    (97, "PossResend"),
    (98, "EncryptMethod"),
    (99, "StopPx"),
    (100, "ExDestination"),
    (102, "CxlRejReason"),
    (103, "OrdRejReason"),
    (108, "HeartBtInt"),
    (110, "MinQty"),
    (111, "MaxFloor"),
    (112, "TestReqID"),
    (122, "OrigSendingTime"),
    (123, "GapFillFlag"),
    (126, "ExpireTime"),
    (141, "ResetSeqNumFlag"),
    (146, "NoRelatedSym"),
    (150, "ExecType"),
    (151, "LeavesQty"),
    (167, "SecurityType"),
    (207, "SecurityExchange"),
    (262, "MDReqID"),
    (263, "SubscriptionRequestType"),
    (264, "MarketDepth"),
    (265, "MDUpdateType"),
    (267, "NoMDEntryTypes"),
    (268, "NoMDEntries"),
    (269, "MDEntryType"),
    (270, "MDEntryPx"),
    (271, "MDEntrySize"),
    (279, "MDUpdateAction"),
    (373, "SessionRejectReason"),
    (434, "CxlRejResponseTo"),
    (447, "PartyIDSource"),
    (448, "PartyID"),
    (452, "PartyRole"),
    (453, "NoPartyIDs"),
    (553, "Username"),
    (554, "Password"),
    (1128, "ApplVerID"),
    (1137, "DefaultApplVerID"),
];

const EDIFACT_SEGMENTS: &[(&str, &str)] = &[
    ("UNA", "Service string advice"),
    ("UNB", "Interchange header"),
    ("UNZ", "Interchange trailer"),
    ("UNG", "Functional group header"),
    ("UNE", "Functional group trailer"),
    ("UNH", "Message header"),
    ("UNT", "Message trailer"),
    ("BGM", "Beginning of message"),
    ("DTM", "Date/time/period"),
    ("NAD", "Name and address"),
    ("RFF", "Reference"),
    ("CUX", "Currencies"),
    ("PAT", "Payment terms basis"),
    ("TOD", "Terms of delivery or transport"),
    ("LOC", "Place/location identification"),
    ("LIN", "Line item"),
    ("PIA", "Additional product id"),
    ("IMD", "Item description"),
    ("QTY", "Quantity"),
    ("PRI", "Price details"),
    ("MOA", "Monetary amount"),
    ("TAX", "Duty/tax/fee details"),
    ("ALC", "Allowance or charge"),
    ("UNS", "Section control"),
    ("CNT", "Control total"),
    ("FTX", "Free text"),
    ("CTA", "Contact information"),
    ("COM", "Communication contact"),
    ("TDT", "Transport information"),
    ("PAC", "Package"),
    ("MEA", "Measurements"),
    ("GIN", "Goods identity number"),
    ("CPS", "Consignment packing sequence"),
    ("FII", "Financial institution information"),
    ("PCD", "Percentage details"),
    ("STS", "Status"),
    ("DOC", "Document/message details"),
    ("GIR", "Related identification numbers"),
    ("EQD", "Equipment details"),
    ("SEL", "Seal number"),
    ("ERC", "Application error information"),
];

fn invalid(standard: &str, message: impl std::fmt::Display) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid {standard} input: {message}"),
    )
}

fn json_of(value: &Value, standard: &str) -> SqlResult<Node> {
    let text = match value {
        Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => text.clone(),
        _ => return Err(invalid(standard, "a JSON document is expected")),
    };
    parse_document(&text).map_err(|error| invalid(standard, error.message))
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn leaf_text(node: &Node) -> String {
    match &node.value {
        Json::Null => String::new(),
        Json::Boolean(flag) => flag.to_string(),
        Json::Number(text) | Json::String(text) => text.clone(),
        Json::Array(_) | Json::Object(_) => String::new(),
    }
}

struct Delimiters {
    field: char,
    component: char,
    repetition: char,
    escape: char,
    subcomponent: char,
}

impl Delimiters {
    fn escaped(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for character in text.chars() {
            let code = if character == self.field {
                Some('F')
            } else if character == self.component {
                Some('S')
            } else if character == self.repetition {
                Some('R')
            } else if character == self.escape {
                Some('E')
            } else if character == self.subcomponent {
                Some('T')
            } else {
                None
            };
            match code {
                Some(code) => {
                    out.push(self.escape);
                    out.push(code);
                    out.push(self.escape);
                }
                None => out.push(character),
            }
        }
        out
    }

    fn joined(&self, node: &Node, separator: char, inner: &dyn Fn(&Node) -> String) -> String {
        match &node.value {
            Json::Array(items) => items
                .iter()
                .map(inner)
                .collect::<Vec<_>>()
                .join(&separator.to_string()),
            _ => self.escaped(&leaf_text(node)),
        }
    }

    fn subcomponents(&self, node: &Node) -> String {
        self.joined(node, self.subcomponent, &|leaf| self.escaped(&leaf_text(leaf)))
    }

    fn components(&self, node: &Node) -> String {
        self.joined(node, self.component, &|part| self.subcomponents(part))
    }

    fn level(&self, node: &Node, _depth: usize) -> String {
        match &node.value {
            Json::Array(items) if items.iter().any(|item| matches!(item.value, Json::Array(_))) => {
                self.joined(node, self.repetition, &|repetition| self.components(repetition))
            }
            _ => self.components(node),
        }
    }
}

fn hl7_build(document: &Node) -> SqlResult<String> {
    let Json::Array(segments) = &document.value else {
        return Err(invalid("HL7", "the message is a JSON array of segments"));
    };
    let mut delimiters = Delimiters {
        field: '|',
        component: '^',
        repetition: '~',
        escape: '\\',
        subcomponent: '&',
    };
    let mut out = Vec::with_capacity(segments.len());
    for (at, segment) in segments.iter().enumerate() {
        let name = member(segment, "segment").map(leaf_text).unwrap_or_default();
        if name.len() != 3 || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(invalid("HL7", format!("{name:?} is not a segment name")));
        }
        let fields: Vec<&Node> = match member(segment, "fields").map(|node| &node.value) {
            Some(Json::Array(fields)) => fields.iter().collect(),
            _ => Vec::new(),
        };
        if at == 0 {
            if name != "MSH" {
                return Err(invalid("HL7", "the first segment must be MSH"));
            }
            let field = fields.first().map(|node| leaf_text(node)).unwrap_or_default();
            let encoding = fields.get(1).map(|node| leaf_text(node)).unwrap_or_default();
            let characters: Vec<char> = field.chars().chain(encoding.chars()).collect();
            if let [field, component, repetition, escape, subcomponent, ..] = characters[..] {
                delimiters = Delimiters {
                    field,
                    component,
                    repetition,
                    escape,
                    subcomponent,
                };
            }
            let mut line = format!(
                "MSH{}{}{}{}{}",
                delimiters.field,
                delimiters.component,
                delimiters.repetition,
                delimiters.escape,
                delimiters.subcomponent
            );
            for field in fields.iter().skip(2) {
                line.push(delimiters.field);
                line.push_str(&delimiters.level(field, 0));
            }
            out.push(line);
            continue;
        }
        let mut line = name;
        for field in fields {
            line.push(delimiters.field);
            line.push_str(&delimiters.level(field, 0));
        }
        out.push(line);
    }
    if out.is_empty() {
        return Err(invalid("HL7", "it has no segments"));
    }
    Ok(out.join("\r") + "\r")
}

fn hl7_field<'t>(header: &'t str, separator: char, number: usize) -> &'t str {
    header.split(separator).nth(number - 1).unwrap_or("")
}

fn hl7_ack(message: &str, code: &str, text: Option<&str>) -> SqlResult<String> {
    if !matches!(code, "AA" | "AE" | "AR" | "CA" | "CE" | "CR") {
        return Err(invalid("HL7", "an acknowledgment code is AA, AE, AR, CA, CE or CR"));
    }
    let header = message
        .split(['\r', '\n'])
        .find(|segment| !segment.trim().is_empty())
        .ok_or_else(|| invalid("HL7", "it is empty"))?;
    if !header.starts_with("MSH") || header.len() < 8 {
        return Err(invalid("HL7", "it does not start with an MSH segment"));
    }
    let field = header[3..].chars().next().unwrap_or('|');
    let encoding: String = header[3 + field.len_utf8()..].chars().take(4).collect();
    let component = encoding.chars().next().unwrap_or('^');
    let get = |number: usize| hl7_field(header, field, number);
    let trigger = get(9).split(component).nth(1).unwrap_or("");
    let control = get(10);
    let now = crate::datetime::utc_to_local(crate::datetime::statement_now())?;
    let stamp = crate::datetime::format_timestamp(now)
        .replace(['-', ':', ' '], "")
        .chars()
        .take(14)
        .collect::<String>();
    let msh = [
        "MSH".to_owned(),
        encoding.clone(),
        get(5).to_owned(),
        get(6).to_owned(),
        get(3).to_owned(),
        get(4).to_owned(),
        stamp,
        String::new(),
        format!("ACK{component}{trigger}{component}ACK"),
        format!("ACK{control}"),
        get(11).to_owned(),
        get(12).to_owned(),
    ]
    .join(&field.to_string());
    let mut msa = format!("MSA{field}{code}{field}{control}");
    if let Some(text) = text {
        msa.push(field);
        msa.push_str(text);
    }
    Ok(format!("{msh}\r{msa}\r"))
}

fn fix_build(document: &Node) -> SqlResult<String> {
    let Json::Array(fields) = &document.value else {
        return Err(invalid("FIX", "the message is a JSON array of {tag, value} fields"));
    };
    let mut begin = String::from("FIX.4.4");
    let mut body = String::new();
    for field in fields {
        let tag = member(field, "tag").map(leaf_text).unwrap_or_default();
        let tag: u32 = tag
            .parse()
            .map_err(|_| invalid("FIX", format!("{tag:?} is not a tag number")))?;
        let value = member(field, "value").map(leaf_text).unwrap_or_default();
        if value.contains(SOH) {
            return Err(invalid("FIX", "a value can't hold the SOH delimiter"));
        }
        match tag {
            8 => begin = value,
            9 | 10 => {}
            _ => {
                body.push_str(&format!("{tag}={value}{SOH}"));
            }
        }
    }
    let head = format!("8={begin}{SOH}9={}{SOH}", body.len());
    let checksum = head
        .bytes()
        .chain(body.bytes())
        .fold(0u32, |sum, byte| sum + u32::from(byte))
        % 256;
    Ok(format!("{head}{body}10={checksum:03}{SOH}"))
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    FUNCTIONS.contains(&name).then_some(SqlType::Text)
}

/// `hl7_build(jsonb)`, `hl7_ack(message [, code [, text]])`, `fix_build(jsonb)`,
/// `fix_tag_name(tag)` and `edifact_segment_name(tag)`.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.first().is_none_or(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some((|| {
        Ok(match (name, values) {
            ("hl7_build", [document]) => Value::Text(hl7_build(&json_of(document, "HL7")?)?),
            ("fix_build", [document]) => Value::Text(fix_build(&json_of(document, "FIX")?)?),
            ("hl7_ack", [message, rest @ ..]) => {
                let message = message
                    .character_text()
                    .ok_or_else(|| invalid("HL7", "the message is text"))?;
                let code = rest
                    .first()
                    .and_then(Value::character_text)
                    .unwrap_or("AA");
                let text = rest.get(1).and_then(Value::character_text);
                Value::Text(hl7_ack(message, code, text)?)
            }
            ("fix_tag_name", [tag]) => tag
                .as_integer()
                .and_then(|tag| {
                    FIX_TAGS
                        .iter()
                        .find(|(number, _)| i64::from(*number) == tag)
                })
                .map_or(Value::Null, |(_, name)| Value::Text((*name).to_owned())),
            ("edifact_segment_name", [tag]) => tag
                .character_text()
                .and_then(|tag| {
                    EDIFACT_SEGMENTS
                        .iter()
                        .find(|(code, _)| code.eq_ignore_ascii_case(tag.trim()))
                })
                .map_or(Value::Null, |(_, name)| Value::Text((*name).to_owned())),
            _ => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("{name} does not take {} arguments", values.len()),
                ));
            }
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_messages_get_their_length_and_checksum() {
        let document = parse_document(
            r#"[{"tag": 8, "value": "FIX.4.2"}, {"tag": 35, "value": "0"}, {"tag": 49, "value": "A"}, {"tag": 56, "value": "B"}, {"tag": 34, "value": "1"}]"#,
        )
        .unwrap();
        let built = fix_build(&document).unwrap();
        assert!(built.starts_with("8=FIX.4.2\u{1}9=20\u{1}35=0\u{1}"));
        let body_and_head = &built[..built.len() - 7];
        let sum = body_and_head.bytes().map(u32::from).sum::<u32>() % 256;
        assert!(built.ends_with(&format!("10={sum:03}\u{1}")));
    }

    #[test]
    fn hl7_messages_rebuild_and_acknowledge() {
        let document = parse_document(
            r#"[{"segment": "MSH", "fields": ["|", "^~\\&", "APP", "FAC", "REC", "RFAC", "20240101", "", ["ADT", "A01"], "MSG1", "P", "2.5"]}, {"segment": "PID", "fields": ["1", "", [["123", "", "", "MRN"]], "", ["DOE", "JANE"]]}]"#,
        )
        .unwrap();
        let built = hl7_build(&document).unwrap();
        assert!(built.starts_with("MSH|^~\\&|APP|FAC|REC|RFAC|20240101||ADT^A01|MSG1|P|2.5\r"));
        assert!(built.contains("PID|1||123^^^MRN||DOE^JANE\r"));
        let ack = hl7_ack(&built, "AA", None).unwrap();
        assert!(ack.contains("|REC|RFAC|APP|FAC|"));
        assert!(ack.contains("ACK^A01^ACK"));
        assert!(ack.ends_with("MSA|AA|MSG1\r"));
    }
}
