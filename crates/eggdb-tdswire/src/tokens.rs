use crate::types::WireColumn;

/// The collation EggDB reports for every character column and the session:
/// Latin1_General_100_BIN2 (LCID 0x0409, the binary-code-point flag, collation version 2,
/// sort id 0), as [MS-TDS] 2.2.5.1.2 lays it out. Comparisons in this version are binary, and
/// this is the binary collation (D122).
pub const COLLATION: [u8; 5] = [0x09, 0x04, 0x00, 0x22, 0x00];

pub(crate) const DONE: u8 = 0xFD;
const COLMETADATA: u8 = 0x81;
const ROW: u8 = 0xD1;
const ERROR: u8 = 0xAA;
const INFO: u8 = 0xAB;
const LOGINACK: u8 = 0xAD;
const ENVCHANGE: u8 = 0xE3;

pub(crate) const DONE_MORE: u16 = 0x0001;
pub(crate) const DONE_ERROR: u16 = 0x0002;
pub(crate) const DONE_IN_TRANSACTION: u16 = 0x0004;
pub(crate) const DONE_COUNT: u16 = 0x0010;
pub(crate) const DONE_ATTENTION: u16 = 0x0020;

pub(crate) const ENV_DATABASE: u8 = 1;
pub(crate) const ENV_LANGUAGE: u8 = 2;
pub(crate) const ENV_PACKET_SIZE: u8 = 4;
const ENV_COLLATION: u8 = 7;
const ENV_BEGIN_TRANSACTION: u8 = 8;
pub(crate) const ENV_COMMIT_TRANSACTION: u8 = 9;
pub(crate) const ENV_ROLLBACK_TRANSACTION: u8 = 10;

pub(crate) const SERVER_NAME: &str = "EggDB";
const MAX_B_VARCHAR_UNITS: usize = 255;
const MAX_US_VARCHAR_UNITS: usize = 4000;

fn units(text: &str, limit: usize) -> Vec<u16> {
    let mut out = Vec::new();
    for character in text.chars() {
        let mut buffer = [0u16; 2];
        let encoded = character.encode_utf16(&mut buffer);
        if out.len() + encoded.len() > limit {
            break;
        }
        out.extend_from_slice(encoded);
    }
    out
}

pub(crate) fn push_b_varchar(out: &mut Vec<u8>, text: &str) {
    let units = units(text, MAX_B_VARCHAR_UNITS);
    out.push(units.len() as u8);
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
}

fn push_us_varchar(out: &mut Vec<u8>, text: &str) {
    let units = units(text, MAX_US_VARCHAR_UNITS);
    out.extend_from_slice(&(units.len() as u16).to_le_bytes());
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
}

fn with_length(token: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 3);
    out.push(token);
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub(crate) fn env_change_text(kind: u8, new: &str, old: &str) -> Vec<u8> {
    let mut body = vec![kind];
    push_b_varchar(&mut body, new);
    push_b_varchar(&mut body, old);
    with_length(ENVCHANGE, body)
}

pub(crate) fn env_change_collation() -> Vec<u8> {
    let mut body = vec![ENV_COLLATION, COLLATION.len() as u8];
    body.extend_from_slice(&COLLATION);
    body.push(0);
    with_length(ENVCHANGE, body)
}

pub(crate) fn env_change_begin(descriptor: u64) -> Vec<u8> {
    let mut body = vec![ENV_BEGIN_TRANSACTION, 8];
    body.extend_from_slice(&descriptor.to_le_bytes());
    body.push(0);
    with_length(ENVCHANGE, body)
}

pub(crate) fn env_change_end(kind: u8, descriptor: u64) -> Vec<u8> {
    let mut body = vec![kind, 0, 8];
    body.extend_from_slice(&descriptor.to_le_bytes());
    with_length(ENVCHANGE, body)
}

pub(crate) struct ServerMessage<'m> {
    pub(crate) number: u32,
    pub(crate) severity: u8,
    pub(crate) text: &'m str,
    pub(crate) line: u32,
}

fn message_token(token: u8, message: &ServerMessage<'_>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&message.number.to_le_bytes());
    body.push(1);
    body.push(message.severity);
    push_us_varchar(&mut body, message.text);
    push_b_varchar(&mut body, SERVER_NAME);
    push_b_varchar(&mut body, "");
    body.extend_from_slice(&message.line.to_le_bytes());
    with_length(token, body)
}

pub(crate) fn error(message: &ServerMessage<'_>) -> Vec<u8> {
    message_token(ERROR, message)
}

pub(crate) fn info(message: &ServerMessage<'_>) -> Vec<u8> {
    message_token(INFO, message)
}

pub(crate) fn login_ack(tds_version: u32, program: &str, version: [u8; 4]) -> Vec<u8> {
    let mut body = vec![1];
    body.extend_from_slice(&tds_version.to_be_bytes());
    push_b_varchar(&mut body, program);
    body.extend_from_slice(&version);
    with_length(LOGINACK, body)
}

pub(crate) fn done(token: u8, status: u16, command: u16, rows: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.push(token);
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(&command.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out
}

pub(crate) fn column_metadata(columns: &[WireColumn]) -> Vec<u8> {
    let mut out = vec![COLMETADATA];
    out.extend_from_slice(&(columns.len() as u16).to_le_bytes());
    for column in columns {
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0x0001u16.to_le_bytes());
        column.kind.push_type_info(&mut out);
        push_b_varchar(&mut out, &column.name);
    }
    out
}

pub(crate) fn row_start() -> u8 {
    ROW
}

#[cfg(test)]
mod tests {
    use super::{
        COLLATION, DONE, DONE_COUNT, DONE_MORE, ENV_COMMIT_TRANSACTION, ServerMessage, done,
        env_change_begin, env_change_end, env_change_text, error, login_ack,
    };

    #[test]
    fn the_collation_is_latin1_general_100_bin2() {
        let value = u32::from_le_bytes([COLLATION[0], COLLATION[1], COLLATION[2], COLLATION[3]]);
        assert_eq!(value & 0x000F_FFFF, 0x0409);
        assert_eq!((value >> 20) & 0xFF, 0x20);
        assert_eq!(value >> 28, 2);
        assert_eq!(COLLATION[4], 0);
    }

    #[test]
    fn tokens_have_the_layouts_of_the_specification() {
        assert_eq!(
            done(DONE, DONE_MORE | DONE_COUNT, 0xC1, 3),
            [0xFD, 0x11, 0x00, 0xC1, 0x00, 3, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            login_ack(0x7400_0004, "EggDB", [16, 0, 3, 0xE8])[..8],
            [0xAD, 20, 0, 1, 0x74, 0x00, 0x00, 0x04]
        );
        assert_eq!(
            env_change_text(4, "4096", "512"),
            [
                0xE3, 17, 0, 4, 4, b'4', 0, b'0', 0, b'9', 0, b'6', 0, 3, b'5', 0, b'1', 0, b'2', 0
            ]
        );
        assert_eq!(
            env_change_begin(1),
            [0xE3, 11, 0, 8, 8, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            env_change_end(ENV_COMMIT_TRANSACTION, 1),
            [0xE3, 11, 0, 9, 0, 8, 1, 0, 0, 0, 0, 0, 0, 0]
        );
        let token = error(&ServerMessage {
            number: 208,
            severity: 16,
            text: "x",
            line: 2,
        });
        assert_eq!(
            token,
            [
                0xAA, 26, 0, 208, 0, 0, 0, 1, 16, 1, 0, b'x', 0, 5, b'E', 0, b'g', 0, b'g', 0,
                b'D', 0, b'B', 0, 0, 2, 0, 0, 0
            ]
        );
    }
}
