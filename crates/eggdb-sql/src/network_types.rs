//! PostgreSQL's network address types `inet`, `cidr`, `macaddr` and `macaddr8` (D1328): each
//! value is checked when it is made and kept in its canonical text form, as the other text-kept
//! types are (D195), and compares as PostgreSQL orders addresses, with the functions that take
//! them apart.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedType, DefinedValue, SqlType, Value};
use std::cmp::Ordering;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub(crate) const INET_OID: u32 = 869;
pub(crate) const CIDR_OID: u32 = 650;
pub(crate) const MACADDR_OID: u32 = 829;
pub(crate) const MACADDR8_OID: u32 = 774;

/// Which network type a value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkKind {
    /// `inet`: a host address with an optional netmask.
    Inet,
    /// `cidr`: a network, with no bits set to the right of its mask.
    Cidr,
    /// `macaddr`: a six-byte hardware address.
    MacAddr,
    /// `macaddr8`: an eight-byte (EUI-64) hardware address.
    MacAddr8,
}

/// A value of a network type, in its canonical text form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkValue {
    /// Its type.
    pub kind: NetworkKind,
    /// Its text, as PostgreSQL prints it.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Address {
    v6: bool,
    bytes: [u8; 16],
    bits: u8,
}

impl Address {
    fn width(self) -> u8 {
        if self.v6 { 128 } else { 32 }
    }

    fn octets(self) -> usize {
        if self.v6 { 16 } else { 4 }
    }

    fn ip(self) -> IpAddr {
        if self.v6 {
            IpAddr::V6(Ipv6Addr::from(self.bytes))
        } else {
            IpAddr::V4(Ipv4Addr::new(self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]))
        }
    }

    fn masked(self, bits: u8) -> Self {
        let mut bytes = self.bytes;
        for (index, byte) in bytes.iter_mut().enumerate().take(self.octets()) {
            let start = (index * 8) as u8;
            *byte &= mask_byte(bits.saturating_sub(start).min(8));
        }
        for byte in bytes.iter_mut().skip(self.octets()) {
            *byte = 0;
        }
        Self { bytes, ..self }
    }

    fn with_host_bits_set(self) -> Self {
        let mut bytes = self.bytes;
        for (index, byte) in bytes.iter_mut().enumerate().take(self.octets()) {
            let start = (index * 8) as u8;
            *byte |= !mask_byte(self.bits.saturating_sub(start).min(8));
        }
        Self { bytes, ..self }
    }

    fn has_host_bits(self) -> bool {
        self.masked(self.bits) != Self { bits: self.bits, ..self }.masked(self.width())
    }

    fn text(self, cidr: bool) -> String {
        if !cidr && self.bits == self.width() {
            self.ip().to_string()
        } else {
            format!("{}/{}", self.ip(), self.bits)
        }
    }
}

fn mask_byte(bits: u8) -> u8 {
    if bits >= 8 { 0xff } else { !(0xffu8 >> bits) }
}

fn invalid(kind: NetworkKind, text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {}: \"{text}\"", type_name(kind)),
    )
}

fn type_name(kind: NetworkKind) -> &'static str {
    match kind {
        NetworkKind::Inet => "inet",
        NetworkKind::Cidr => "cidr",
        NetworkKind::MacAddr => "macaddr",
        NetworkKind::MacAddr8 => "macaddr8",
    }
}

fn abbreviated_v4(text: &str) -> Option<([u8; 16], usize)> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        bytes[index] = part.parse().ok()?;
    }
    Some((bytes, parts.len()))
}

fn address(text: &str, kind: NetworkKind) -> SqlResult<Address> {
    let trimmed = text.trim();
    let (written, bits) = match trimmed.split_once('/') {
        Some((written, bits)) => (written, Some(bits)),
        None => (trimmed, None),
    };
    let bits = match bits {
        Some(bits) => Some(bits.parse::<u8>().map_err(|_| invalid(kind, text))?),
        None => None,
    };
    let (v6, bytes, octets_written) = match written.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&v4.octets());
            (false, bytes, 4)
        }
        Ok(IpAddr::V6(v6)) => (true, v6.octets(), 16),
        Err(_) if kind == NetworkKind::Cidr => {
            let (bytes, written) = abbreviated_v4(written).ok_or_else(|| invalid(kind, text))?;
            (false, bytes, written)
        }
        Err(_) => return Err(invalid(kind, text)),
    };
    let width = if v6 { 128 } else { 32 };
    let bits = match bits {
        Some(bits) if bits > width => return Err(invalid(kind, text)),
        Some(bits) => bits,
        None if kind == NetworkKind::Cidr && !v6 => {
            let first = bytes[0];
            let classful: u8 = if first >= 240 {
                32
            } else if first >= 224 {
                8
            } else if first >= 192 {
                24
            } else if first >= 128 {
                16
            } else {
                8
            };
            let widened = classful.max((octets_written * 8) as u8);
            if widened == 8 && first == 224 { 4 } else { widened }
        }
        None => width,
    };
    Ok(Address { v6, bytes, bits })
}

fn parsed_address(value: &NetworkValue) -> Option<Address> {
    match value.kind {
        NetworkKind::Inet | NetworkKind::Cidr => address(&value.text, NetworkKind::Inet).ok(),
        NetworkKind::MacAddr | NetworkKind::MacAddr8 => None,
    }
}

fn hardware(text: &str, kind: NetworkKind) -> SqlResult<String> {
    let digits: String = text
        .trim()
        .chars()
        .filter(|character| !matches!(character, ':' | '-' | '.'))
        .collect();
    if !digits.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(invalid(kind, text));
    }
    let digits = digits.to_ascii_lowercase();
    let digits = match (kind, digits.len()) {
        (NetworkKind::MacAddr, 12) | (NetworkKind::MacAddr8, 16) => digits,
        (NetworkKind::MacAddr8, 12) => format!("{}fffe{}", &digits[..6], &digits[6..]),
        _ => return Err(invalid(kind, text)),
    };
    Ok(digits
        .as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).into_owned())
        .collect::<Vec<_>>()
        .join(":"))
}

/// The value of network type `kind` written as `text`.
pub(crate) fn parse(text: &str, kind: NetworkKind) -> SqlResult<NetworkValue> {
    let canonical = match kind {
        NetworkKind::Inet => address(text, kind)?.text(false),
        NetworkKind::Cidr => {
            let found = address(text, kind)?;
            if found.has_host_bits() {
                return Err(SqlError::new(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    format!("invalid cidr value: \"{text}\""),
                )
                .with_detail("Value has bits set to right of mask."));
            }
            found.text(true)
        }
        NetworkKind::MacAddr | NetworkKind::MacAddr8 => hardware(text, kind)?,
    };
    Ok(NetworkValue { kind, text: canonical })
}

pub(crate) fn kind_of(defined: DefinedType) -> Option<NetworkKind> {
    if defined.kind != crate::types::DefinedKind::Base {
        return None;
    }
    kind_of_oid(defined.oid)
}

pub(crate) fn kind_of_oid(oid: u32) -> Option<NetworkKind> {
    match oid {
        INET_OID => Some(NetworkKind::Inet),
        CIDR_OID => Some(NetworkKind::Cidr),
        MACADDR_OID => Some(NetworkKind::MacAddr),
        MACADDR8_OID => Some(NetworkKind::MacAddr8),
        _ => None,
    }
}

pub(crate) fn defined_type(kind: NetworkKind) -> DefinedType {
    match kind {
        NetworkKind::Inet => DefinedType::INET,
        NetworkKind::Cidr => DefinedType::CIDR,
        NetworkKind::MacAddr => DefinedType::MACADDR,
        NetworkKind::MacAddr8 => DefinedType::MACADDR8,
    }
}

pub(crate) fn value(network: NetworkValue) -> Value {
    Value::Defined(Box::new(DefinedValue::Network(network)))
}

/// Orders two network values as PostgreSQL does: addresses by family, then by their common
/// network bits, then by mask length, then by the whole address; hardware addresses by bytes.
pub(crate) fn compare(left: &NetworkValue, right: &NetworkValue) -> Ordering {
    match (parsed_address(left), parsed_address(right)) {
        (Some(a), Some(b)) => a
            .v6
            .cmp(&b.v6)
            .then_with(|| {
                let common = a.bits.min(b.bits);
                a.masked(common).bytes.cmp(&b.masked(common).bytes)
            })
            .then(a.bits.cmp(&b.bits))
            .then(a.bytes.cmp(&b.bytes)),
        _ => left.kind.cmp_rank().cmp(&right.kind.cmp_rank()).then_with(|| left.text.cmp(&right.text)),
    }
}

/// A byte key that sorts as [`compare`] orders the values, for index keys and equality.
pub(crate) fn ordered_key(network: &NetworkValue) -> Vec<u8> {
    let Some(found) = parsed_address(network) else {
        return network.text.as_bytes().to_vec();
    };
    let mut key = vec![u8::from(found.v6)];
    for bit in 0..usize::from(found.bits) {
        let byte = found.bytes[bit / 8];
        key.push(1 + ((byte >> (7 - bit % 8)) & 1));
    }
    key.push(0);
    key.extend_from_slice(&found.bytes[..found.octets()]);
    key
}

impl NetworkKind {
    fn cmp_rank(self) -> u8 {
        match self {
            Self::Inet | Self::Cidr => 0,
            Self::MacAddr => 1,
            Self::MacAddr8 => 2,
        }
    }
}

/// Converts between the network types, as `::inet`, `::cidr` and `::macaddr8` do.
pub(crate) fn convert(network: &NetworkValue, target: NetworkKind) -> SqlResult<NetworkValue> {
    match (network.kind, target) {
        (from, to) if from == to => Ok(network.clone()),
        (NetworkKind::Cidr, NetworkKind::Inet) => {
            let found = address(&network.text, NetworkKind::Inet)?;
            Ok(NetworkValue { kind: target, text: found.text(false) })
        }
        (NetworkKind::Inet, NetworkKind::Cidr) => {
            let found = address(&network.text, NetworkKind::Inet)?;
            Ok(NetworkValue { kind: target, text: found.masked(found.bits).text(true) })
        }
        (NetworkKind::MacAddr, NetworkKind::MacAddr8) => parse(&network.text, NetworkKind::MacAddr8),
        (NetworkKind::MacAddr8, NetworkKind::MacAddr) => {
            let bytes: Vec<&str> = network.text.split(':').collect();
            if bytes.get(3..5) != Some(&["ff", "fe"][..]) {
                return Err(SqlError::new(
                    SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                    "macaddr8 data out of range to convert to macaddr",
                )
                .with_hint("Only addresses that have FF and FE as values in the 4th and 5th bytes from the left, for example xx:xx:xx:ff:fe:xx:xx:xx, are eligible to be converted from macaddr8 to macaddr."));
            }
            Ok(NetworkValue {
                kind: target,
                text: [&bytes[..3], &bytes[5..]].concat().join(":"),
            })
        }
        _ => Err(SqlError::new(
            SqlState::CANNOT_COERCE,
            format!("cannot cast type {} to {}", type_name(network.kind), type_name(target)),
        )),
    }
}

const FAMILY_INET: u8 = 2;
const FAMILY_INET6: u8 = 3;

/// The network value inside `value`, if it holds one.
pub(crate) fn network_of(value: &Value) -> Option<&NetworkValue> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Network(network) => Some(network),
            _ => None,
        },
        _ => None,
    }
}

fn hardware_bytes(text: &str) -> Vec<u8> {
    text.split(':').filter_map(|pair| u8::from_str_radix(pair, 16).ok()).collect()
}

/// The value in PostgreSQL's binary form: family, mask bits, whether it is a `cidr`, the
/// address length and the address for `inet` and `cidr`; the bytes for hardware addresses.
pub(crate) fn binary(network: &NetworkValue) -> Option<Vec<u8>> {
    match network.kind {
        NetworkKind::MacAddr | NetworkKind::MacAddr8 => Some(hardware_bytes(&network.text)),
        NetworkKind::Inet | NetworkKind::Cidr => {
            let found = parsed_address(network)?;
            let mut bytes = vec![
                if found.v6 { FAMILY_INET6 } else { FAMILY_INET },
                found.bits,
                u8::from(network.kind == NetworkKind::Cidr),
                found.octets() as u8,
            ];
            bytes.extend_from_slice(&found.bytes[..found.octets()]);
            Some(bytes)
        }
    }
}

/// The text of a value of type `kind` sent in PostgreSQL's binary form, or `None` when the
/// bytes do not fit it.
pub(crate) fn text_from_binary(bytes: &[u8], kind: NetworkKind) -> Option<String> {
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(":");
    match kind {
        NetworkKind::MacAddr => (bytes.len() == 6).then(|| hex(bytes)),
        NetworkKind::MacAddr8 => (bytes.len() == 8).then(|| hex(bytes)),
        NetworkKind::Inet | NetworkKind::Cidr => {
            let [family, bits, _, length, address @ ..] = bytes else {
                return None;
            };
            let v6 = match (*family, *length, address.len()) {
                (FAMILY_INET, 4, 4) => false,
                (FAMILY_INET6, 16, 16) => true,
                _ => return None,
            };
            let mut stored = [0u8; 16];
            stored[..address.len()].copy_from_slice(address);
            let found = Address { v6, bytes: stored, bits: *bits };
            if *bits > found.width() || (kind == NetworkKind::Cidr && found.has_host_bits()) {
                return None;
            }
            Some(found.text(kind == NetworkKind::Cidr))
        }
    }
}

/// The functions on network addresses, by name.
pub(crate) const FUNCTIONS: [&str; 17] = [
    "host", "masklen", "set_masklen", "netmask", "hostmask", "broadcast", "network", "family", "abbrev",
    "inet_same_family", "inet_merge", "macaddr8_set7bit", "network_sub", "network_subeq", "network_sup",
    "network_supeq", "network_overlap",
];

/// PostgreSQL's operators `<<=` and `>>=`, with the functions they call.
pub(crate) const OPERATORS: [(&str, &str); 2] = [("<<=", "network_subeq"), (">>=", "network_supeq")];

/// The type a network function returns, when `name` is one whose type does not follow its
/// argument.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    let defined = |kind| Some(SqlType::Defined(defined_type(kind)));
    match name {
        "host" | "abbrev" => Some(SqlType::Text),
        "masklen" | "family" => Some(SqlType::Integer),
        "inet_same_family" | "network_sub" | "network_subeq" | "network_sup" | "network_supeq"
        | "network_overlap" => Some(SqlType::Boolean),
        "netmask" | "hostmask" | "broadcast" => defined(NetworkKind::Inet),
        "network" | "inet_merge" => defined(NetworkKind::Cidr),
        "macaddr8_set7bit" => defined(NetworkKind::MacAddr8),
        _ => None,
    }
}

/// The range operators `<<`, `>>` and `&&` when both sides are addresses.
pub(crate) fn range_operator(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let operator = match name {
        crate::ranges::LEFT_OF => "<<",
        crate::ranges::RIGHT_OF => ">>",
        crate::ranges::OVERLAPS => "&&",
        _ => return None,
    };
    match values {
        [left, right] => contains(operator, left, right),
        _ => None,
    }
}

fn argument(value: &Value, kind: NetworkKind) -> SqlResult<Option<NetworkValue>> {
    match value {
        Value::Null => Ok(None),
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Network(network) => Ok(Some(network.clone())),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "the function takes a network address")),
        },
        Value::Text(text) => parse(text, kind).map(Some),
        _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "the function takes a network address")),
    }
}

fn ip_argument(value: &Value) -> SqlResult<Option<(NetworkValue, Address)>> {
    let Some(network) = argument(value, NetworkKind::Inet)? else {
        return Ok(None);
    };
    let found = parsed_address(&network)
        .ok_or_else(|| SqlError::new(SqlState::UNDEFINED_FUNCTION, "the function takes an inet or cidr value"))?;
    Ok(Some((network, found)))
}

fn inet(found: Address) -> Value {
    value(NetworkValue { kind: NetworkKind::Inet, text: found.text(false) })
}

/// A network function's answer, or `None` when `name` is not one.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    Some(answer(name, values))
}

fn answer(name: &str, values: &[Value]) -> SqlResult<Value> {
    let operator = match name {
        "network_sub" => Some("<<"),
        "network_subeq" => Some("<<="),
        "network_sup" => Some(">>"),
        "network_supeq" => Some(">>="),
        "network_overlap" => Some("&&"),
        _ => None,
    };
    if let Some(operator) = operator {
        let [left, right] = values else {
            return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes two addresses")));
        };
        if left.is_null() || right.is_null() {
            return Ok(Value::Null);
        }
        let as_address = |value: &Value| argument(value, NetworkKind::Inet).map(|network| network.map(self::value));
        let (left, right) = (as_address(left)?, as_address(right)?);
        let (Some(left), Some(right)) = (left, right) else {
            return Ok(Value::Null);
        };
        return contains(operator, &left, &right).unwrap_or_else(|| {
            Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes inet or cidr values")))
        });
    }
    if name == "macaddr8_set7bit" {
        let Some(network) = values.first().map(|value| argument(value, NetworkKind::MacAddr8)).transpose()?.flatten() else {
            return Ok(Value::Null);
        };
        let mut bytes: Vec<String> = network.text.split(':').map(str::to_owned).collect();
        if let Some(first) = bytes.first_mut()
            && let Ok(number) = u8::from_str_radix(first, 16)
        {
            *first = format!("{:02x}", number | 0x02);
        }
        return Ok(value(NetworkValue { kind: network.kind, text: bytes.join(":") }));
    }
    let Some((network, found)) = values.first().map(ip_argument).transpose()?.flatten() else {
        return Ok(Value::Null);
    };
    Ok(match name {
        "host" => Value::Text(found.ip().to_string()),
        "masklen" => Value::Integer(i32::from(found.bits)),
        "family" => Value::Integer(if found.v6 { 6 } else { 4 }),
        "abbrev" if network.kind == NetworkKind::Cidr => {
            let significant = usize::from(found.bits).div_ceil(8).max(1);
            if found.v6 {
                Value::Text(network.text.clone())
            } else {
                let octets = found.bytes[..significant.min(4)]
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(".");
                Value::Text(format!("{octets}/{}", found.bits))
            }
        }
        "abbrev" => Value::Text(network.text.clone()),
        "netmask" => {
            let all = Address { bytes: [0xff; 16], bits: found.width(), ..found };
            inet(all.masked(found.bits))
        }
        "hostmask" => {
            let none = Address { bytes: [0; 16], ..found };
            let set = none.with_host_bits_set();
            inet(Address { bits: found.width(), ..set })
        }
        "broadcast" => inet(found.with_host_bits_set()),
        "network" => value(NetworkValue { kind: NetworkKind::Cidr, text: found.masked(found.bits).text(true) }),
        "set_masklen" => {
            let Some(bits) = values.get(1).and_then(Value::as_integer) else {
                return Ok(Value::Null);
            };
            let bits = if bits == -1 { i64::from(found.width()) } else { bits };
            let bits = u8::try_from(bits)
                .ok()
                .filter(|bits| *bits <= found.width())
                .ok_or_else(|| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid mask length: {bits}")))?;
            let changed = Address { bits, ..found };
            if network.kind == NetworkKind::Cidr {
                value(NetworkValue { kind: NetworkKind::Cidr, text: changed.masked(bits).text(true) })
            } else {
                inet(changed)
            }
        }
        "inet_same_family" | "inet_merge" => {
            let Some((_, other)) = values.get(1).map(ip_argument).transpose()?.flatten() else {
                return Ok(Value::Null);
            };
            if name == "inet_same_family" {
                return Ok(Value::Boolean(found.v6 == other.v6));
            }
            if found.v6 != other.v6 {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "cannot merge addresses from different families",
                ));
            }
            let mut bits = found.bits.min(other.bits);
            while bits > 0 && found.masked(bits).bytes != other.masked(bits).bytes {
                bits -= 1;
            }
            let merged = Address { bits, ..found }.masked(bits);
            value(NetworkValue { kind: NetworkKind::Cidr, text: merged.text(true) })
        }
        _ => Value::Null,
    })
}

/// The containment operators `<<`, `<<=`, `>>`, `>>=` and `&&` on two addresses, or `None`
/// when either side is no address.
pub(crate) fn contains(operator: &str, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    if network_of(left).is_none() && network_of(right).is_none() {
        return None;
    }
    if left.is_null() || right.is_null() {
        return Some(Ok(Value::Null));
    }
    let address_of = |value: &Value| match value {
        Value::Text(text) => parse(text, NetworkKind::Inet).ok().and_then(|network| parsed_address(&network)),
        other => network_of(other).and_then(parsed_address),
    };
    let (a, b) = (address_of(left)?, address_of(right)?);
    let within = |inner: Address, outer: Address, strict: bool| {
        inner.v6 == outer.v6
            && (if strict { inner.bits > outer.bits } else { inner.bits >= outer.bits })
            && inner.masked(outer.bits).bytes == outer.masked(outer.bits).bytes
    };
    Some(Ok(Value::Boolean(match operator {
        "<<" => within(a, b, true),
        "<<=" => within(a, b, false),
        ">>" => within(b, a, true),
        ">>=" => within(b, a, false),
        "&&" => within(a, b, false) || within(b, a, false),
        _ => return None,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(text: &str, kind: NetworkKind) -> String {
        parse(text, kind).unwrap().text
    }

    #[test]
    fn addresses_take_their_canonical_forms() {
        assert_eq!(text_of("192.168.100.128/25", NetworkKind::Inet), "192.168.100.128/25");
        assert_eq!(text_of("10.1.2.3/32", NetworkKind::Inet), "10.1.2.3");
        assert_eq!(text_of("2001:0db8:0000::1", NetworkKind::Inet), "2001:db8::1");
        assert_eq!(text_of("192.168.1", NetworkKind::Cidr), "192.168.1.0/24");
        assert_eq!(text_of("128.1", NetworkKind::Cidr), "128.1.0.0/16");
        assert_eq!(text_of("10", NetworkKind::Cidr), "10.0.0.0/8");
        assert_eq!(text_of("10.1.2.3", NetworkKind::Cidr), "10.1.2.3/32");
        assert!(parse("192.168.1.2/24", NetworkKind::Cidr).is_err());
        assert!(parse("300.1.1.1", NetworkKind::Inet).is_err());
        assert_eq!(text_of("08-00-2B-01-02-03", NetworkKind::MacAddr), "08:00:2b:01:02:03");
        assert_eq!(text_of("0800.2b01.0203", NetworkKind::MacAddr), "08:00:2b:01:02:03");
        assert_eq!(text_of("08:00:2b:01:02:03", NetworkKind::MacAddr8), "08:00:2b:ff:fe:01:02:03");
    }

    #[test]
    fn addresses_order_by_network_then_mask() {
        let inet = |text: &str| parse(text, NetworkKind::Inet).unwrap();
        assert_eq!(compare(&inet("10.0.0.9"), &inet("10.0.0.10")), Ordering::Less);
        assert_eq!(compare(&inet("10.0.0.0/8"), &inet("10.0.0.0/16")), Ordering::Less);
        assert_eq!(compare(&inet("255.0.0.1"), &inet("::1")), Ordering::Less);
        let pairs = [("10.0.0.9", "10.0.0.10"), ("10.0.0.0/8", "10.0.0.0/16"), ("10.0.0.0/16", "9.0.0.0"), ("1.2.3.4", "::")];
        for (left, right) in pairs {
            assert_eq!(
                ordered_key(&inet(left)).cmp(&ordered_key(&inet(right))),
                compare(&inet(left), &inet(right)),
                "{left} {right}"
            );
        }
    }

    #[test]
    fn the_functions_take_addresses_apart() {
        let inet = |text: &str| value(parse(text, NetworkKind::Inet).unwrap());
        let shown = |name: &str, values: &[Value]| function(name, values).unwrap().unwrap().to_text().unwrap_or_default();
        assert_eq!(shown("host", &[inet("192.168.1.5/24")]), "192.168.1.5");
        assert_eq!(shown("masklen", &[inet("192.168.1.5/24")]), "24");
        assert_eq!(shown("broadcast", &[inet("192.168.1.5/24")]), "192.168.1.255/24");
        assert_eq!(shown("network", &[inet("192.168.1.5/24")]), "192.168.1.0/24");
        assert_eq!(shown("netmask", &[inet("192.168.1.5/24")]), "255.255.255.0");
        assert_eq!(shown("hostmask", &[inet("192.168.23.20/30")]), "0.0.0.3");
        assert_eq!(shown("set_masklen", &[inet("192.168.1.5/24"), Value::Integer(16)]), "192.168.1.5/16");
        assert_eq!(shown("inet_merge", &[inet("192.168.1.5/24"), inet("192.168.2.5/24")]), "192.168.0.0/22");
        assert_eq!(shown("family", &[inet("::1")]), "6");
        let cidr = value(parse("10.1.0.0/16", NetworkKind::Cidr).unwrap());
        assert_eq!(shown("abbrev", &[cidr]), "10.1/16");
        let inside = contains("<<", &inet("192.168.1.5"), &value(parse("192.168.1/24", NetworkKind::Cidr).unwrap()));
        assert_eq!(inside.unwrap().unwrap(), Value::Boolean(true));
        assert!(contains("<<", &Value::Integer(1), &Value::Integer(2)).is_none());
    }

    #[test]
    fn the_binary_form_round_trips() {
        for (text, kind) in [
            ("192.168.1.5/24", NetworkKind::Inet),
            ("2001:db8::/32", NetworkKind::Cidr),
            ("08:00:2b:01:02:03", NetworkKind::MacAddr),
            ("08:00:2b:ff:fe:01:02:03", NetworkKind::MacAddr8),
        ] {
            let network = parse(text, kind).unwrap();
            let bytes = binary(&network).unwrap();
            assert_eq!(text_from_binary(&bytes, kind).as_deref(), Some(network.text.as_str()));
        }
        assert_eq!(binary(&parse("10.0.0.1", NetworkKind::Inet).unwrap()).unwrap(), [2, 32, 0, 4, 10, 0, 0, 1]);
        assert!(text_from_binary(&[2, 24, 1, 4, 10, 0, 0, 1], NetworkKind::Cidr).is_none());
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn columns_keep_addresses_and_queries_order_and_test_them() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE hosts (id integer PRIMARY KEY, ip inet, net cidr, mac macaddr);
             CREATE INDEX hosts_ip ON hosts (ip);
             INSERT INTO hosts VALUES (1, '10.0.0.10', '10.0.0.0/8', '08-00-2B-01-02-03'),
                                      (2, '10.0.0.9/32', '192.168.1', '0800.2b01.0204'),
                                      (3, '::1', '2001:db8::/32', NULL)",
        );
        assert_eq!(
            answer(&mut session, "SELECT id, ip, net, mac FROM hosts ORDER BY ip"),
            "id|ip|net|mac\n2|10.0.0.9|192.168.1.0/24|08:00:2b:01:02:04\n1|10.0.0.10|10.0.0.0/8|08:00:2b:01:02:03\n3|::1|2001:db8::/32|NULL\nSELECT 3"
        );
        assert_eq!(
            answer(&mut session, "SELECT id FROM hosts WHERE ip << net ORDER BY id"),
            "id\n1\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT id FROM hosts WHERE ip <<= '10.0.0.0/8'::cidr ORDER BY id"),
            "id\n1\n2\nSELECT 2"
        );
        assert_eq!(
            answer(&mut session, "SELECT pg_typeof(ip), host(ip), masklen(net), network('10.1.2.3/16'::inet) FROM hosts WHERE id = 1"),
            "pg_typeof|host|masklen|network\ninet|10.0.0.10|8|10.1.0.0/16\nSELECT 1"
        );
        let refused = session.execute("SELECT '192.168.1.2/24'::cidr", &mut |_| {}).unwrap_err();
        assert_eq!(refused.state(), SqlState::INVALID_TEXT_REPRESENTATION);
    }
}
