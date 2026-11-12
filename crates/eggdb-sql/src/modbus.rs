use crate::error::{SqlError, SqlResult, SqlState};

const MBAP_BYTES: usize = 7;
const MOST_FRAME_BYTES: usize = 260;
const EXCEPTION_FLAG: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Request,
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    Tcp,
    Rtu,
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("modbus_decode: {}", message.into()))
}

fn function_name(code: u8) -> &'static str {
    match code {
        1 => "read_coils",
        2 => "read_discrete_inputs",
        3 => "read_holding_registers",
        4 => "read_input_registers",
        5 => "write_single_coil",
        6 => "write_single_register",
        15 => "write_multiple_coils",
        16 => "write_multiple_registers",
        _ => "unknown",
    }
}

fn exception_name(code: u8) -> &'static str {
    match code {
        1 => "illegal_function",
        2 => "illegal_data_address",
        3 => "illegal_data_value",
        4 => "server_device_failure",
        5 => "acknowledge",
        6 => "server_device_busy",
        8 => "memory_parity_error",
        10 => "gateway_path_unavailable",
        11 => "gateway_target_failed_to_respond",
        _ => "unknown",
    }
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

fn word(bytes: &[u8], at: usize) -> SqlResult<u16> {
    bytes.get(at..at + 2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).ok_or_else(|| invalid("the frame is cut"))
}

fn bits(data: &[u8]) -> String {
    let flags: Vec<&str> = data.iter().flat_map(|byte| (0..8).map(move |bit| if byte >> bit & 1 == 1 { "true" } else { "false" })).collect();
    format!("[{}]", flags.join(", "))
}

fn registers(data: &[u8]) -> SqlResult<String> {
    if data.len() % 2 != 0 {
        return Err(invalid("register data has an odd byte count"));
    }
    let values: Vec<String> = data.chunks(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]]).to_string()).collect();
    Ok(format!("[{}]", values.join(", ")))
}

fn counted_data<'a>(pdu: &'a [u8], at: usize) -> SqlResult<&'a [u8]> {
    let count = usize::from(*pdu.get(at).ok_or_else(|| invalid("the frame is cut"))?);
    let data = pdu.get(at + 1..at + 1 + count).ok_or_else(|| invalid("the byte count runs past the frame"))?;
    if at + 1 + count != pdu.len() {
        return Err(invalid("the frame has bytes after its data"));
    }
    Ok(data)
}

fn pdu_fields(pdu: &[u8], direction: Direction) -> SqlResult<Vec<String>> {
    let code = *pdu.first().ok_or_else(|| invalid("the frame has no function code"))?;
    if code & EXCEPTION_FLAG != 0 && direction == Direction::Response {
        let function = code & !EXCEPTION_FLAG;
        let exception = *pdu.get(1).ok_or_else(|| invalid("the exception has no code"))?;
        return Ok(vec![
            format!("\"function\": {function}"),
            format!("\"function_name\": \"{}\"", function_name(function)),
            format!("\"exception\": {exception}"),
            format!("\"exception_name\": \"{}\"", exception_name(exception)),
        ]);
    }
    let mut fields = vec![format!("\"function\": {code}"), format!("\"function_name\": \"{}\"", function_name(code))];
    match (code, direction) {
        (1..=4, Direction::Request) | (15 | 16, Direction::Response) => {
            fields.push(format!("\"address\": {}", word(pdu, 1)?));
            fields.push(format!("\"quantity\": {}", word(pdu, 3)?));
        }
        (1 | 2, Direction::Response) => fields.push(format!("\"coils\": {}", bits(counted_data(pdu, 1)?))),
        (3 | 4, Direction::Response) => fields.push(format!("\"registers\": {}", registers(counted_data(pdu, 1)?)?)),
        (5, _) => {
            fields.push(format!("\"address\": {}", word(pdu, 1)?));
            let value = word(pdu, 3)?;
            if value != 0xFF00 && value != 0 {
                return Err(invalid("a single coil is written as FF00 or 0000"));
            }
            fields.push(format!("\"value\": {}", value == 0xFF00));
        }
        (6, _) => {
            fields.push(format!("\"address\": {}", word(pdu, 1)?));
            fields.push(format!("\"value\": {}", word(pdu, 3)?));
        }
        (15, Direction::Request) => {
            fields.push(format!("\"address\": {}", word(pdu, 1)?));
            let quantity = word(pdu, 3)?;
            fields.push(format!("\"quantity\": {quantity}"));
            let all = bits(counted_data(pdu, 5)?);
            let kept: Vec<&str> = all.trim_matches(['[', ']']).split(", ").take(usize::from(quantity)).collect();
            fields.push(format!("\"coils\": [{}]", kept.join(", ")));
        }
        (16, Direction::Request) => {
            fields.push(format!("\"address\": {}", word(pdu, 1)?));
            fields.push(format!("\"quantity\": {}", word(pdu, 3)?));
            fields.push(format!("\"registers\": {}", registers(counted_data(pdu, 5)?)?));
        }
        _ => fields.push(format!("\"data\": \"{}\"", pdu[1..].iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
    }
    Ok(fields)
}

/// Decodes one Modbus frame (Modbus TCP with its MBAP header, or RTU with its unit address and
/// CRC) as a request or a response, into JSON: the transaction and unit, the function and its
/// fields (addresses, quantities, coils as booleans, registers as numbers), or an exception.
pub(crate) fn decode(frame: &[u8], direction: Direction, framing: Framing) -> SqlResult<String> {
    if frame.len() > MOST_FRAME_BYTES {
        return Err(invalid(format!("a frame is at most {MOST_FRAME_BYTES} bytes")));
    }
    let mut fields = Vec::new();
    let pdu = match framing {
        Framing::Tcp => {
            if frame.len() < MBAP_BYTES + 1 {
                return Err(invalid("a Modbus TCP frame is at least 8 bytes"));
            }
            if word(frame, 2)? != 0 {
                return Err(invalid("the protocol identifier is not 0"));
            }
            let length = usize::from(word(frame, 4)?);
            if length != frame.len() - 6 {
                return Err(invalid("the MBAP length does not match the frame"));
            }
            fields.push(format!("\"transaction\": {}", word(frame, 0)?));
            fields.push(format!("\"unit\": {}", frame[6]));
            &frame[MBAP_BYTES..]
        }
        Framing::Rtu => {
            if frame.len() < 4 {
                return Err(invalid("a Modbus RTU frame is at least 4 bytes"));
            }
            let (body, check) = frame.split_at(frame.len() - 2);
            if crc16(body) != u16::from_le_bytes([check[0], check[1]]) {
                return Err(invalid("the CRC does not match"));
            }
            fields.push(format!("\"unit\": {}", frame[0]));
            &body[1..]
        }
    };
    fields.extend(pdu_fields(pdu, direction)?);
    Ok(format!("{{{}}}", fields.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_writes_and_exceptions_decode() {
        assert_eq!(
            decode(&[0, 1, 0, 0, 0, 6, 17, 3, 0, 107, 0, 3], Direction::Request, Framing::Tcp).unwrap(),
            "{\"transaction\": 1, \"unit\": 17, \"function\": 3, \"function_name\": \"read_holding_registers\", \"address\": 107, \"quantity\": 3}"
        );
        assert_eq!(
            decode(&[0, 1, 0, 0, 0, 9, 17, 3, 6, 2, 43, 0, 0, 0, 100], Direction::Response, Framing::Tcp).unwrap(),
            "{\"transaction\": 1, \"unit\": 17, \"function\": 3, \"function_name\": \"read_holding_registers\", \"registers\": [555, 0, 100]}"
        );
        assert!(decode(&[0, 1, 0, 0, 0, 3, 17, 0x83, 2], Direction::Response, Framing::Tcp).unwrap().contains("\"exception_name\": \"illegal_data_address\""));
        let mut rtu = vec![17, 6, 0, 1, 0, 3];
        let crc = crc16(&rtu).to_le_bytes();
        rtu.extend_from_slice(&crc);
        assert!(decode(&rtu, Direction::Request, Framing::Rtu).unwrap().ends_with("\"address\": 1, \"value\": 3}"));
        rtu[2] ^= 1;
        assert!(decode(&rtu, Direction::Request, Framing::Rtu).is_err());
    }
}
