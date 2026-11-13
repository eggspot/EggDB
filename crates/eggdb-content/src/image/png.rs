use super::{Image, MOST_PIXELS, failure};
use crate::ContentError;

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const ADAM7: [(usize, usize, usize, usize); 7] = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

pub(crate) fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(SIGNATURE)
}

struct Header {
    width: usize,
    height: usize,
    depth: u8,
    color: u8,
    interlaced: bool,
}

impl Header {
    fn channels(&self) -> usize {
        match self.color {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            _ => 4,
        }
    }

    fn bits_per_pixel(&self) -> usize {
        self.channels() * usize::from(self.depth)
    }

    fn row_bytes(&self, width: usize) -> usize {
        (width * self.bits_per_pixel()).div_ceil(8)
    }
}

fn paeth(left: u8, above: u8, corner: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(above) - i16::from(corner);
    let (to_left, to_above, to_corner) = ((estimate - i16::from(left)).abs(), (estimate - i16::from(above)).abs(), (estimate - i16::from(corner)).abs());
    if to_left <= to_above && to_left <= to_corner {
        left
    } else if to_above <= to_corner {
        above
    } else {
        corner
    }
}

fn unfilter(data: &[u8], header: &Header, width: usize, height: usize) -> Result<(Vec<u8>, usize), ContentError> {
    let row = header.row_bytes(width);
    let step = header.bits_per_pixel().div_ceil(8).max(1);
    let needed = (row + 1).checked_mul(height).ok_or_else(|| failure("the PNG is too large"))?;
    let rows = data.get(..needed).ok_or_else(|| failure("the PNG's image data is cut short"))?;
    let mut out = vec![0u8; row * height];
    for y in 0..height {
        let filter = rows[y * (row + 1)];
        let source = &rows[y * (row + 1) + 1..(y + 1) * (row + 1)];
        for x in 0..row {
            let left = if x >= step { out[y * row + x - step] } else { 0 };
            let above = if y > 0 { out[(y - 1) * row + x] } else { 0 };
            let corner = if y > 0 && x >= step { out[(y - 1) * row + x - step] } else { 0 };
            out[y * row + x] = match filter {
                0 => source[x],
                1 => source[x].wrapping_add(left),
                2 => source[x].wrapping_add(above),
                3 => source[x].wrapping_add(((u16::from(left) + u16::from(above)) / 2) as u8),
                4 => source[x].wrapping_add(paeth(left, above, corner)),
                other => return Err(failure(format!("the PNG uses filter {other}"))),
            };
        }
    }
    Ok((out, needed))
}

fn sample(row: &[u8], header: &Header, x: usize, channel: usize) -> u16 {
    let depth = usize::from(header.depth);
    if depth == 16 {
        let at = (x * header.channels() + channel) * 2;
        return u16::from_be_bytes([row[at], row[at + 1]]);
    }
    if depth == 8 {
        return u16::from(row[x * header.channels() + channel]);
    }
    let bit = x * depth;
    let byte = row[bit / 8];
    let shift = 8 - depth - bit % 8;
    u16::from((byte >> shift) & ((1u8 << depth) - 1))
}

fn to_eight(value: u16, depth: u8) -> u8 {
    match depth {
        16 => (value >> 8) as u8,
        8 => value as u8,
        depth => ((u32::from(value) * 255) / ((1u32 << depth) - 1)) as u8,
    }
}

struct Palette {
    colors: Vec<[u8; 3]>,
    alpha: Vec<u8>,
    key: Option<[u16; 3]>,
}

fn pixel(row: &[u8], header: &Header, palette: &Palette, x: usize) -> Result<[u8; 4], ContentError> {
    let depth = header.depth;
    Ok(match header.color {
        0 => {
            let gray = sample(row, header, x, 0);
            let alpha = if palette.key.is_some_and(|key| key[0] == gray) { 0 } else { 255 };
            let value = to_eight(gray, depth);
            [value, value, value, alpha]
        }
        2 => {
            let (red, green, blue) = (sample(row, header, x, 0), sample(row, header, x, 1), sample(row, header, x, 2));
            let alpha = if palette.key == Some([red, green, blue]) { 0 } else { 255 };
            [to_eight(red, depth), to_eight(green, depth), to_eight(blue, depth), alpha]
        }
        3 => {
            let index = usize::from(sample(row, header, x, 0));
            let color = palette.colors.get(index).ok_or_else(|| failure("a PNG pixel names a color its palette lacks"))?;
            [color[0], color[1], color[2], palette.alpha.get(index).copied().unwrap_or(255)]
        }
        4 => {
            let value = to_eight(sample(row, header, x, 0), depth);
            [value, value, value, to_eight(sample(row, header, x, 1), depth)]
        }
        _ => [
            to_eight(sample(row, header, x, 0), depth),
            to_eight(sample(row, header, x, 1), depth),
            to_eight(sample(row, header, x, 2), depth),
            to_eight(sample(row, header, x, 3), depth),
        ],
    })
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Image, ContentError> {
    let mut at = SIGNATURE.len();
    let mut header = None;
    let mut palette = Palette { colors: Vec::new(), alpha: Vec::new(), key: None };
    let mut compressed = Vec::new();
    while at + 8 <= bytes.len() {
        let length = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let kind = &bytes[at + 4..at + 8];
        let data = bytes.get(at + 8..at + 8 + length).ok_or_else(|| failure("a PNG chunk is cut short"))?;
        let stored_crc = bytes.get(at + 8 + length..at + 12 + length).ok_or_else(|| failure("a PNG chunk has no CRC"))?;
        if crate::crc32(&bytes[at + 4..at + 8 + length]).to_be_bytes() != stored_crc {
            return Err(failure("a PNG chunk's CRC does not match"));
        }
        at += 12 + length;
        match kind {
            b"IHDR" if data.len() == 13 => {
                let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
                let (depth, color) = (data[8], data[9]);
                let valid = matches!((color, depth), (0, 1 | 2 | 4 | 8 | 16) | (3, 1 | 2 | 4 | 8) | (2 | 4 | 6, 8 | 16));
                if !valid || data[10] != 0 || data[11] != 0 || data[12] > 1 {
                    return Err(failure("the PNG's header is not valid"));
                }
                if width == 0 || height == 0 || width.saturating_mul(height) > MOST_PIXELS {
                    return Err(failure("the PNG is empty or larger than the renditions read"));
                }
                header = Some(Header { width, height, depth, color, interlaced: data[12] == 1 });
            }
            b"PLTE" => palette.colors = data.chunks_exact(3).map(|rgb| [rgb[0], rgb[1], rgb[2]]).collect(),
            b"tRNS" => match header.as_ref().map(|header| header.color) {
                Some(3) => palette.alpha = data.to_vec(),
                Some(0) if data.len() >= 2 => palette.key = Some([u16::from_be_bytes([data[0], data[1]]); 3]),
                Some(2) if data.len() >= 6 => {
                    palette.key = Some([u16::from_be_bytes([data[0], data[1]]), u16::from_be_bytes([data[2], data[3]]), u16::from_be_bytes([data[4], data[5]])]);
                }
                _ => {}
            },
            b"IDAT" => compressed.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
    }
    let header = header.ok_or_else(|| failure("the PNG has no header"))?;
    let passes: Vec<(usize, usize, usize, usize)> = if header.interlaced { ADAM7.to_vec() } else { vec![(0, 0, 1, 1)] };
    let expected: usize = passes
        .iter()
        .map(|(x0, y0, dx, dy)| {
            let (width, height) = ((header.width + dx - 1 - x0) / dx, (header.height + dy - 1 - y0) / dy);
            if width == 0 || height == 0 { 0 } else { (header.row_bytes(width) + 1) * height }
        })
        .sum();
    let raw = crate::zlib_inflate(&compressed, expected).map_err(|error| failure(format!("the PNG's image data cannot be read: {error:?}")))?;
    let mut image = Image { width: header.width, height: header.height, rgba: vec![0u8; header.width * header.height * 4] };
    let mut offset = 0;
    for (x0, y0, dx, dy) in passes {
        let (width, height) = ((header.width + dx - 1 - x0) / dx, (header.height + dy - 1 - y0) / dy);
        if width == 0 || height == 0 {
            continue;
        }
        let (rows, used) = unfilter(raw.get(offset..).unwrap_or_default(), &header, width, height)?;
        offset += used;
        let row_bytes = header.row_bytes(width);
        for y in 0..height {
            let row = &rows[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..width {
                let (out_x, out_y) = (x0 + x * dx, y0 + y * dy);
                let at = (out_y * header.width + out_x) * 4;
                image.rgba[at..at + 4].copy_from_slice(&pixel(row, &header, &palette, x)?);
            }
        }
    }
    Ok(image)
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crate::crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// `image` as an 8-bit RGBA PNG, each row filtered with `Sub` and the rows deflated.
pub(crate) fn encode(image: &Image) -> Vec<u8> {
    let mut raw = Vec::with_capacity(image.height * (image.width * 4 + 1));
    for row in image.rgba.chunks_exact(image.width * 4) {
        raw.push(1);
        for (at, &byte) in row.iter().enumerate() {
            raw.push(byte.wrapping_sub(if at >= 4 { row[at - 4] } else { 0 }));
        }
    }
    let mut zlib = vec![0x78, 0x01];
    zlib.extend_from_slice(&crate::deflate(&raw));
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(image.width as u32).to_be_bytes());
    header.extend_from_slice(&(image.height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = SIGNATURE.to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}
